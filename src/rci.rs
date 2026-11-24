//! Minimal RCI client for Keenetic routers.
//!
//! RCI is the router's local JSON control interface. We speak HTTP/1.1 over a
//! loopback TCP socket; no TLS, no async runtime, no curl. The client reads
//! interface state and changes PPPoE `up` state. Every write is verified by a
//! readback, and the working request shape is probed and cached because
//! firmware versions accept slightly different RCI forms.

use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

const MAX_BODY: usize = 1024 * 1024;
const DEFAULT_PORT: u16 = 79;

#[derive(Debug)]
pub enum RciError {
    Io(String),
    Http { status: u16, body: String },
    Unauthorized,
    Json(String),
    Parse(String),
    NotFound(String),
    WriteFailed { interface: String, attempts: String },
}

impl fmt::Display for RciError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RciError::Io(err) => write!(formatter, "RCI connection failed: {err}"),
            RciError::Http { status, body } => {
                write!(formatter, "RCI returned HTTP {status}: {body}")
            }
            RciError::Unauthorized => write!(
                formatter,
                "RCI requires an access token (HTTP 401/403); run `wolguard rci token` \
                 or set rci.token in the configuration"
            ),
            RciError::Json(err) => write!(formatter, "cannot parse RCI JSON: {err}"),
            RciError::Parse(err) => write!(formatter, "cannot parse RCI response: {err}"),
            RciError::NotFound(id) => write!(formatter, "interface {id:?} is not reported by RCI"),
            RciError::WriteFailed {
                interface,
                attempts,
            } => write!(
                formatter,
                "cannot change interface {interface:?}: no RCI write form took effect ({attempts})"
            ),
        }
    }
}

impl std::error::Error for RciError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RciEndpoint {
    pub host: String,
    pub port: u16,
}

impl RciEndpoint {
    pub fn parse(input: &str) -> Result<RciEndpoint, String> {
        let text = input.trim();
        if text.is_empty() {
            return Err("RCI URL must not be empty".to_string());
        }
        if text.starts_with("https://") {
            return Err(format!("RCI URL must use http://, got {text:?}"));
        }
        let rest = text.strip_prefix("http://").unwrap_or(text);
        let rest = rest.trim_end_matches('/');
        if rest.contains('/') {
            return Err(format!("RCI URL must not contain a path, got {text:?}"));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| format!("invalid RCI port in {text:?}"))?;
                (host, port)
            }
            Some(_) => return Err(format!("invalid RCI URL {text:?}")),
            None => (rest, DEFAULT_PORT),
        };
        if host.is_empty() {
            return Err(format!("invalid RCI URL {text:?}"));
        }
        Ok(RciEndpoint {
            host: host.to_string(),
            port,
        })
    }

    fn address(&self) -> Result<SocketAddr, RciError> {
        let mut addresses = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|err| RciError::Io(err.to_string()))?;
        addresses
            .next()
            .ok_or_else(|| RciError::Io(format!("cannot resolve {}:{}", self.host, self.port)))
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct DhcpBinding {
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub interface: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireSummary {
    #[serde(default)]
    layer: WireLayer,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WireLayer {
    #[serde(default)]
    ipv4: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireInterface {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "interface-name", default)]
    interface_name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    connected: Value,
    #[serde(default)]
    address: Value,
    #[serde(default)]
    mask: Value,
    #[serde(default)]
    uptime: Option<u64>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    summary: Option<WireSummary>,
    #[serde(rename = "last-change", default)]
    last_change: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouterInterface {
    pub id: String,
    pub interface_name: String,
    pub kind: String,
    pub description: String,
    pub state: String,
    pub link: String,
    pub connected: String,
    pub address: Option<Ipv4Addr>,
    pub mask: Option<Ipv4Addr>,
    pub uptime: Option<u64>,
    pub priority: Option<i32>,
    pub summary_ipv4: Option<String>,
    pub last_change: Option<String>,
}

impl RouterInterface {
    fn from_wire(key: &str, wire: WireInterface) -> RouterInterface {
        RouterInterface {
            id: wire.id.unwrap_or_else(|| key.to_string()),
            interface_name: wire.interface_name,
            kind: wire.kind,
            description: wire.description,
            state: wire.state,
            link: wire.link,
            connected: normalize_connected(&wire.connected),
            address: parse_address(&wire.address),
            mask: parse_address(&wire.mask),
            uptime: wire.uptime,
            priority: wire.priority,
            summary_ipv4: wire
                .summary
                .and_then(|summary| summary.layer.ipv4)
                .filter(|value| !value.is_empty()),
            last_change: value_to_string(wire.last_change),
        }
    }

    pub fn is_up(&self) -> bool {
        self.state.eq_ignore_ascii_case("up")
            && self.link.eq_ignore_ascii_case("up")
            && self.connected.eq_ignore_ascii_case("yes")
    }

    /// True when IPv4 is actually running and an address is assigned. A
    /// missing `summary` block is normal on PPPoE firmware and is accepted.
    pub fn is_usable(&self) -> bool {
        self.is_up()
            && self.address.is_some()
            && self
                .summary_ipv4
                .as_deref()
                .map(|value| value.eq_ignore_ascii_case("running"))
                .unwrap_or(true)
    }

    pub fn is_pppoe(&self) -> bool {
        self.kind.eq_ignore_ascii_case("PPPoE")
    }
}

fn normalize_connected(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(flag) = value.as_bool() {
        return if flag { "yes" } else { "no" }.to_string();
    }
    String::new()
}

fn parse_address(value: &Value) -> Option<Ipv4Addr> {
    let text = value.as_str()?.trim();
    if text.is_empty() || text == "0.0.0.0" {
        return None;
    }
    text.parse().ok()
}

fn value_to_string(value: Option<Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteForm {
    /// `POST /rci/interface/<id>` with a string flag (the form used by kvas).
    InterfacePathString,
    /// `POST /rci/interface/<id>` with a boolean flag.
    InterfacePathBool,
    /// `POST /rci/interface` with a boolean flag.
    InterfaceMapBool,
    /// `POST /rci/interface` with a string flag.
    InterfaceMapString,
    /// `POST /rci/` with a boolean flag.
    RciNestedBool,
    /// `POST /rci/` with a string flag.
    RciNestedString,
}

impl WriteForm {
    fn label(self) -> &'static str {
        match self {
            WriteForm::InterfacePathString => "POST /rci/interface/<id> (string)",
            WriteForm::InterfacePathBool => "POST /rci/interface/<id> (bool)",
            WriteForm::InterfaceMapBool => "POST /rci/interface (bool)",
            WriteForm::InterfaceMapString => "POST /rci/interface (string)",
            WriteForm::RciNestedBool => "POST /rci/ (bool)",
            WriteForm::RciNestedString => "POST /rci/ (string)",
        }
    }

    fn all() -> [WriteForm; 6] {
        [
            WriteForm::InterfacePathString,
            WriteForm::InterfacePathBool,
            WriteForm::InterfaceMapBool,
            WriteForm::InterfaceMapString,
            WriteForm::RciNestedBool,
            WriteForm::RciNestedString,
        ]
    }

    fn body(self, id: &str, up: bool) -> (String, String) {
        let boolean = if up { "true" } else { "false" };
        let string = if up { "\"true\"" } else { "\"false\"" };
        let path = format!("/rci/interface/{}", encode_path_segment(id));
        let escaped = escape_json(id);
        match self {
            WriteForm::InterfacePathString => (path, format!("{{\"up\":{string}}}")),
            WriteForm::InterfacePathBool => (path, format!("{{\"up\":{boolean}}}")),
            WriteForm::InterfaceMapBool => (
                "/rci/interface".to_string(),
                format!("{{\"{escaped}\":{{\"up\":{boolean}}}}}"),
            ),
            WriteForm::InterfaceMapString => (
                "/rci/interface".to_string(),
                format!("{{\"{escaped}\":{{\"up\":{string}}}}}"),
            ),
            WriteForm::RciNestedBool => (
                "/rci/".to_string(),
                format!("{{\"interface\":{{\"{escaped}\":{{\"up\":{boolean}}}}}}}"),
            ),
            WriteForm::RciNestedString => (
                "/rci/".to_string(),
                format!("{{\"interface\":{{\"{escaped}\":{{\"up\":{string}}}}}}}"),
            ),
        }
    }
}

pub struct RciClient {
    endpoint: RciEndpoint,
    token: String,
    timeout: Duration,
    working_form: Option<WriteForm>,
}

impl RciClient {
    pub fn new(endpoint: RciEndpoint, token: String, timeout: Duration) -> RciClient {
        RciClient {
            endpoint,
            token,
            timeout,
            working_form: None,
        }
    }

    pub fn from_url(url: &str, token: &str, timeout: Duration) -> Result<RciClient, String> {
        Ok(RciClient::new(
            RciEndpoint::parse(url)?,
            token.to_string(),
            timeout,
        ))
    }

    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    #[cfg(test)]
    pub fn working_form(&self) -> Option<WriteForm> {
        self.working_form
    }

    pub fn list_interfaces(&mut self) -> Result<Vec<RouterInterface>, RciError> {
        let response = self.request("GET", "/rci/show/interface/", None)?;
        parse_interface_list(&response)
    }

    pub fn list_pppoe(&mut self) -> Result<Vec<RouterInterface>, RciError> {
        let mut interfaces = self.list_interfaces()?;
        interfaces.retain(RouterInterface::is_pppoe);
        interfaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(interfaces)
    }

    pub fn set_interface_up(&mut self, id: &str, up: bool) -> Result<(), RciError> {
        let current = self
            .list_interfaces()?
            .into_iter()
            .find(|interface| interface.id == id)
            .ok_or_else(|| RciError::NotFound(id.to_string()))?;
        if up == current.is_up() {
            return Ok(());
        }

        let mut candidates: Vec<WriteForm> = Vec::new();
        if let Some(form) = self.working_form {
            candidates.push(form);
        }
        for form in WriteForm::all() {
            if !candidates.contains(&form) {
                candidates.push(form);
            }
        }

        let mut attempts = Vec::new();
        for form in candidates {
            let (path, body) = form.body(id, up);
            match self.request("POST", &path, Some(&body)) {
                Ok(_) => {}
                Err(RciError::Unauthorized) => return Err(RciError::Unauthorized),
                Err(err) => {
                    attempts.push(format!("{}: {err}", form.label()));
                    continue;
                }
            }
            if self.verify_state(id, up)? {
                self.working_form = Some(form);
                return Ok(());
            }
            attempts.push(format!("{}: no state change", form.label()));
        }
        Err(RciError::WriteFailed {
            interface: id.to_string(),
            attempts: attempts.join("; "),
        })
    }

    fn verify_state(&mut self, id: &str, up: bool) -> Result<bool, RciError> {
        let deadline = Instant::now()
            + if up {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(1)
            };
        loop {
            let interfaces = self.list_interfaces()?;
            if let Some(interface) = interfaces.iter().find(|interface| interface.id == id) {
                if interface.is_up() == up {
                    return Ok(true);
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn dhcp_bindings(&mut self) -> Result<Vec<DhcpBinding>, RciError> {
        let response = self.request("GET", "/rci/show/ip/dhcp/bindings", None)?;
        parse_bindings(&response)
    }

    pub fn version(&mut self) -> Result<String, RciError> {
        let response = self.request("GET", "/rci/show/version", None)?;
        let value: Value =
            serde_json::from_slice(&response).map_err(|err| RciError::Json(err.to_string()))?;
        Ok(value
            .get("release")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Vec<u8>, RciError> {
        let address = self.endpoint.address()?;
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)
            .map_err(|err| RciError::Io(err.to_string()))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|err| RciError::Io(err.to_string()))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|err| RciError::Io(err.to_string()))?;

        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.endpoint.host, self.endpoint.port
        );
        if !self.token.trim().is_empty() {
            request.push_str("X-NDMA-TKN: ");
            request.push_str(self.token.trim());
            request.push_str("\r\n");
        }
        if let Some(body) = body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }
        stream
            .write_all(request.as_bytes())
            .map_err(|err| RciError::Io(err.to_string()))?;
        stream
            .flush()
            .map_err(|err| RciError::Io(err.to_string()))?;

        let mut raw = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    raw.extend_from_slice(&buffer[..count]);
                    if raw.len() > MAX_BODY {
                        return Err(RciError::Parse("RCI response is too large".to_string()));
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::Interrupted
                        || err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if raw.is_empty() {
                        return Err(RciError::Io(err.to_string()));
                    }
                    break;
                }
                Err(err) => return Err(RciError::Io(err.to_string())),
            }
        }

        let response = parse_http_response(&raw)?;
        if response.status == 401 || response.status == 403 {
            return Err(RciError::Unauthorized);
        }
        if !(200..300).contains(&response.status) {
            let body = String::from_utf8_lossy(&response.body);
            let snippet: String = body.chars().take(300).collect();
            return Err(RciError::Http {
                status: response.status,
                body: snippet,
            });
        }
        Ok(response.body)
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, RciError> {
    let (head_end, body_start) = find_header_end(raw)
        .ok_or_else(|| RciError::Parse("missing HTTP header terminator".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| RciError::Parse("empty HTTP response".to_string()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| RciError::Parse(format!("bad HTTP status line {status_line:?}")))?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }

    let body = &raw[body_start..];
    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(length) = content_length {
        if body.len() < length {
            return Err(RciError::Parse(format!(
                "truncated HTTP body: expected {length} bytes, got {}",
                body.len()
            )));
        }
        body[..length].to_vec()
    } else {
        body.to_vec()
    };
    Ok(HttpResponse { status, body })
}

fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    if let Some(index) = find_subslice(raw, b"\r\n\r\n") {
        return Some((index, index + 4));
    }
    find_subslice(raw, b"\n\n").map(|index| (index, index + 2))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, RciError> {
    let mut output = Vec::new();
    loop {
        let line_end = find_subslice(data, b"\r\n")
            .or_else(|| find_subslice(data, b"\n"))
            .ok_or_else(|| RciError::Parse("bad chunked encoding".to_string()))?;
        let line = String::from_utf8_lossy(&data[..line_end]);
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| RciError::Parse(format!("bad chunk size {size_text:?}")))?;
        let skip = if data.get(line_end) == Some(&b'\r') {
            line_end + 2
        } else {
            line_end + 1
        };
        data = &data[skip..];
        if size == 0 {
            return Ok(output);
        }
        if data.len() < size {
            return Err(RciError::Parse("truncated chunk".to_string()));
        }
        output.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        } else if data.starts_with(b"\n") {
            data = &data[1..];
        }
    }
}

fn parse_interface_list(body: &[u8]) -> Result<Vec<RouterInterface>, RciError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|err| RciError::Json(err.to_string()))?;
    let map = value
        .as_object()
        .ok_or_else(|| RciError::Json("interface list is not a JSON object".to_string()))?;
    let map = map
        .get("show")
        .and_then(|show| show.get("interface"))
        .and_then(Value::as_object)
        .unwrap_or(map);

    let mut interfaces = Vec::with_capacity(map.len());
    for (key, entry) in map {
        if !entry.is_object() {
            continue;
        }
        let wire: WireInterface = match serde_json::from_value(entry.clone()) {
            Ok(wire) => wire,
            Err(_) => continue,
        };
        interfaces.push(RouterInterface::from_wire(key, wire));
    }
    interfaces.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(interfaces)
}

fn parse_bindings(body: &[u8]) -> Result<Vec<DhcpBinding>, RciError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|err| RciError::Json(err.to_string()))?;
    let list = value
        .get("lease")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| RciError::Json("DHCP bindings do not contain a lease array".to_string()))?;
    let mut bindings = Vec::with_capacity(list.len());
    for entry in list {
        if let Ok(binding) = serde_json::from_value::<DhcpBinding>(entry.clone()) {
            bindings.push(binding);
        }
    }
    Ok(bindings)
}

fn encode_path_segment(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn escape_json(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push(' '),
            ch => output.push(ch),
        }
    }
    output
}

/// Generate a Keenetic RCI access token through `ndmc` and return it.
/// `ndmc` is the vendor CLI; this is used only for token bootstrap.
pub fn generate_token(ndmc_path: &str) -> Result<String, String> {
    let output = run_ndmc(ndmc_path, "authentication token generate wolguard")?;
    let clean = strip_ansi(&output);
    let token = parse_token_output(&clean)
        .ok_or_else(|| "ndmc generated a token but its output has no value: line".to_string())?;
    // Persist the token in the router configuration, best effort.
    let _ = run_ndmc(ndmc_path, "system configuration save");
    Ok(token)
}

fn run_ndmc(ndmc_path: &str, command: &str) -> Result<String, String> {
    let candidates: Vec<String> = if ndmc_path == "ndmc" {
        vec![
            "ndmc".to_string(),
            "/bin/ndmc".to_string(),
            "/usr/sbin/ndmc".to_string(),
            "/opt/bin/ndmc".to_string(),
        ]
    } else {
        vec![ndmc_path.to_string()]
    };
    let mut last_error = String::new();
    for candidate in candidates {
        match Command::new(&candidate).arg("-c").arg(command).output() {
            Ok(output) => {
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).to_string());
                }
                last_error = format!(
                    "{candidate} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(err) => last_error = format!("{candidate}: {err}"),
        }
    }
    Err(format!("cannot run ndmc: {last_error}"))
}

fn parse_token_output(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(value) = trimmed.strip_prefix("value:") else {
            continue;
        };
        let token = value.trim();
        if token.len() >= 8 && token.len() <= 512 && token.chars().all(|ch| ch.is_ascii_graphic()) {
            return Some(token.to_string());
        }
    }
    None
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch != '\r' {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn parses_endpoint_urls() {
        assert_eq!(
            RciEndpoint::parse("http://127.0.0.1:79").unwrap(),
            RciEndpoint {
                host: "127.0.0.1".to_string(),
                port: 79
            }
        );
        assert_eq!(
            RciEndpoint::parse("127.0.0.1:8080").unwrap(),
            RciEndpoint {
                host: "127.0.0.1".to_string(),
                port: 8080
            }
        );
        assert_eq!(
            RciEndpoint::parse("http://localhost").unwrap(),
            RciEndpoint {
                host: "localhost".to_string(),
                port: 79
            }
        );
        assert!(RciEndpoint::parse("http://127.0.0.1:79/rci").is_err());
        assert!(RciEndpoint::parse("https://127.0.0.1").is_err());
    }

    #[test]
    fn parses_content_length_and_chunked_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");

        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.body, b"hello");

        let raw = b"HTTP/1.1 200 OK\r\n\r\nplain";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.body, b"plain");
    }

    #[test]
    fn parses_pppoe_interface_without_summary() {
        let json = br#"{
          "PPPoE0": {
            "id": "PPPoE0",
            "interface-name": "ppp0",
            "type": "PPPoE",
            "description": "Provider",
            "state": "up",
            "link": "up",
            "connected": "yes",
            "address": "100.70.12.34",
            "mask": "255.255.255.255",
            "uptime": 1234
          }
        }"#;
        let interfaces = parse_interface_list(json).unwrap();
        assert_eq!(interfaces.len(), 1);
        let interface = &interfaces[0];
        assert_eq!(interface.id, "PPPoE0");
        assert_eq!(interface.interface_name, "ppp0");
        assert_eq!(interface.kind, "PPPoE");
        assert!(interface.is_up());
        assert!(interface.is_usable());
        assert_eq!(interface.address, Some(Ipv4Addr::new(100, 70, 12, 34)));
        assert_eq!(interface.uptime, Some(1234));
        assert_eq!(interface.summary_ipv4, None);
    }

    #[test]
    fn parses_working_summary_and_empty_address() {
        let json = br#"{
          "PPPoE0": {
            "id": "PPPoE0",
            "type": "PPPoE",
            "state": "up",
            "link": "up",
            "connected": true,
            "address": "0.0.0.0",
            "summary": {"layer": {"ipv4": "pending"}}
          }
        }"#;
        let interfaces = parse_interface_list(json).unwrap();
        let interface = &interfaces[0];
        assert!(interface.is_up());
        assert!(!interface.is_usable());
        assert_eq!(interface.address, None);
        assert_eq!(interface.summary_ipv4.as_deref(), Some("pending"));
    }

    #[test]
    fn parses_dhcp_bindings() {
        let json = br#"{"lease":[{"ip":"192.168.1.10","mac":"aa:bb:cc:dd:ee:ff","name":"server","hostname":"server","interface":"Bridge0"}]}"#;
        let bindings = parse_bindings(json).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].ip, "192.168.1.10");
        assert_eq!(bindings[0].name, "server");
        assert_eq!(bindings[0].interface, "Bridge0");
    }

    #[test]
    fn parses_token_output_and_strips_ansi() {
        let text = "\u{1b}[1m value: abcdef1234567890\u{1b}[0m\r\n";
        assert_eq!(
            parse_token_output(&strip_ansi(text)).as_deref(),
            Some("abcdef1234567890")
        );
        assert_eq!(parse_token_output("nothing here"), None);
    }

    fn detected_form(path: &str, body: &str) -> Option<&'static str> {
        let lower = body.to_ascii_lowercase();
        let string = lower.contains("\"up\":\"true\"") || lower.contains("\"up\":\"false\"");
        let boolean = lower.contains("\"up\":true") || lower.contains("\"up\":false");
        match path {
            "/rci/interface/PPPoE0" if string => Some("path-string"),
            "/rci/interface/PPPoE0" if boolean => Some("path-bool"),
            "/rci/interface" if string => Some("map-string"),
            "/rci/interface" if boolean => Some("map-bool"),
            "/rci/" if string => Some("nested-string"),
            "/rci/" if boolean => Some("nested-bool"),
            _ => None,
        }
    }

    struct FakeRouter {
        up: Arc<AtomicBool>,
        form: Arc<std::sync::Mutex<String>>,
        stop: Arc<AtomicBool>,
        address: SocketAddr,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeRouter {
        fn start(apply_form: &'static str) -> FakeRouter {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let up = Arc::new(AtomicBool::new(false));
            let form = Arc::new(std::sync::Mutex::new(String::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let up_clone = up.clone();
            let form_clone = form.clone();
            let stop_clone = stop.clone();
            let handle = std::thread::spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut reader = BufReader::new(stream.try_clone().unwrap());
                            let mut request_line = String::new();
                            if reader.read_line(&mut request_line).is_err() {
                                continue;
                            }
                            let mut content_length = 0usize;
                            loop {
                                let mut line = String::new();
                                if reader.read_line(&mut line).is_err() || line == "\r\n" {
                                    break;
                                }
                                if let Some(value) = line.strip_prefix("Content-Length:") {
                                    content_length = value.trim().parse().unwrap_or(0);
                                }
                            }
                            let mut body = vec![0u8; content_length];
                            if content_length > 0 {
                                use std::io::Read as _;
                                let _ = reader.read_exact(&mut body);
                            }
                            let body = String::from_utf8_lossy(&body).to_string();
                            let method = request_line.split_whitespace().next().unwrap_or("");
                            let path = request_line.split_whitespace().nth(1).unwrap_or("");
                            if method == "POST" {
                                *form_clone.lock().unwrap() = path.to_string();
                                let changes = detected_form(path, &body) == Some(apply_form);
                                if changes {
                                    let lower = body.to_ascii_lowercase();
                                    let want_up = lower.contains("\"up\":true")
                                        || lower.contains("\"up\":\"true\"");
                                    let want_down = lower.contains("\"up\":false")
                                        || lower.contains("\"up\":\"false\"");
                                    if want_up {
                                        up_clone.store(true, Ordering::Relaxed);
                                    } else if want_down {
                                        up_clone.store(false, Ordering::Relaxed);
                                    }
                                }
                            }
                            let is_up = up_clone.load(Ordering::Relaxed);
                            let json = format!(
                                "{{\"PPPoE0\":{{\"id\":\"PPPoE0\",\"interface-name\":\"ppp0\",\"type\":\"PPPoE\",\"description\":\"Test\",\"state\":\"{}\",\"link\":\"{}\",\"connected\":\"{}\",\"address\":\"{}\",\"mask\":\"255.255.255.255\",\"uptime\":1}}}}",
                                if is_up { "up" } else { "down" },
                                if is_up { "up" } else { "down" },
                                if is_up { "yes" } else { "no" },
                                if is_up { "100.70.12.34" } else { "" }
                            );
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                json.len(),
                                json
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            FakeRouter {
                up,
                form,
                stop,
                address,
                handle: Some(handle),
            }
        }

        fn client(&self) -> RciClient {
            RciClient::new(
                RciEndpoint {
                    host: "127.0.0.1".to_string(),
                    port: self.address.port(),
                },
                String::new(),
                Duration::from_secs(2),
            )
        }
    }

    impl Drop for FakeRouter {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn uses_the_kvas_string_write_form_first() {
        let router = FakeRouter::start("path-string");
        let mut client = router.client();
        client.set_interface_up("PPPoE0", true).unwrap();
        assert!(router.up.load(Ordering::Relaxed));
        assert_eq!(client.working_form(), Some(WriteForm::InterfacePathString));
        assert_eq!(*router.form.lock().unwrap(), "/rci/interface/PPPoE0");
    }

    #[test]
    fn falls_back_to_the_boolean_path_form() {
        let router = FakeRouter::start("path-bool");
        let mut client = router.client();
        client.set_interface_up("PPPoE0", true).unwrap();
        assert!(router.up.load(Ordering::Relaxed));
        assert_eq!(client.working_form(), Some(WriteForm::InterfacePathBool));
        assert_eq!(*router.form.lock().unwrap(), "/rci/interface/PPPoE0");
    }

    #[test]
    fn falls_back_to_the_documented_write_form() {
        let router = FakeRouter::start("map-bool");
        let mut client = router.client();
        client.set_interface_up("PPPoE0", true).unwrap();
        assert!(router.up.load(Ordering::Relaxed));
        assert_eq!(client.working_form(), Some(WriteForm::InterfaceMapBool));
        assert_eq!(*router.form.lock().unwrap(), "/rci/interface");
    }

    #[test]
    fn falls_back_to_the_nested_write_form() {
        let router = FakeRouter::start("nested-bool");
        let mut client = router.client();
        client.set_interface_up("PPPoE0", true).unwrap();
        assert!(router.up.load(Ordering::Relaxed));
        assert_eq!(client.working_form(), Some(WriteForm::RciNestedBool));
        assert_eq!(*router.form.lock().unwrap(), "/rci/");
    }

    #[test]
    fn no_effect_is_a_write_failure() {
        let router = FakeRouter::start("none");
        let mut client = router.client();
        let err = client.set_interface_up("PPPoE0", true).unwrap_err();
        assert!(matches!(err, RciError::WriteFailed { .. }), "{err}");
    }
}
