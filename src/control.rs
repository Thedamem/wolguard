//! Local control socket: the TUI/CLI client protocol.
//!
//! One request per connection, newline-delimited JSON. The daemon owns the
//! socket; clients never start a second monitor.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::model::{Command, SharedLogs, SharedSnapshot};
use crate::wan::WanCommand;

pub const PROTO_VERSION: u32 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "default_proto")]
    pub proto: u32,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

fn default_proto() -> u32 {
    PROTO_VERSION
}

impl Request {
    pub fn new(cmd: &str) -> Request {
        Request {
            proto: PROTO_VERSION,
            cmd: cmd.to_string(),
            args: Value::Null,
        }
    }

    pub fn with_args(cmd: &str, args: Value) -> Request {
        Request {
            proto: PROTO_VERSION,
            cmd: cmd.to_string(),
            args,
        }
    }

    fn args<T: serde::de::DeserializeOwned>(&self) -> Result<T, Response> {
        serde_json::from_value(self.args.clone()).map_err(|err| {
            Response::error(
                "bad_request",
                format!("invalid arguments for {}: {err}", self.cmd),
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default = "default_proto")]
    pub proto: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl Response {
    pub fn ok(data: Value) -> Response {
        Response {
            ok: true,
            proto: PROTO_VERSION,
            data: Some(data),
            error: None,
            kind: None,
        }
    }

    pub fn message(message: impl Into<String>) -> Response {
        Response::ok(json!({ "message": message.into() }))
    }

    pub fn error(kind: &str, message: impl Into<String>) -> Response {
        Response {
            ok: false,
            proto: PROTO_VERSION,
            data: None,
            error: Some(message.into()),
            kind: Some(kind.to_string()),
        }
    }
}

pub struct Control {
    pub snapshot: SharedSnapshot,
    pub logs: SharedLogs,
    host_tx: Mutex<Sender<Command>>,
    wan_tx: Mutex<Sender<WanCommand>>,
    config_path: PathBuf,
    config: Mutex<Config>,
}

impl Control {
    pub fn new(
        snapshot: SharedSnapshot,
        logs: SharedLogs,
        host_tx: Sender<Command>,
        wan_tx: Sender<WanCommand>,
        config: Config,
    ) -> Control {
        Control {
            snapshot,
            logs,
            host_tx: Mutex::new(host_tx),
            wan_tx: Mutex::new(wan_tx),
            config_path: PathBuf::from("/opt/etc/wolguard.conf"),
            config: Mutex::new(config),
        }
    }

    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Control {
        self.config_path = path.into();
        self
    }

    pub fn handle(&self, request: &Request) -> Response {
        match request.cmd.as_str() {
            "ping" => Response::ok(json!({ "pong": true })),
            "status" => self.status(),
            "logs" => self.logs(request),
            "wake" => self.wake(request),
            "pppoe-scan" => self.pppoe_scan(),
            "pppoe-restart" => self.pppoe_restart(request),
            "reload" => self.reload(),
            other => Response::error("unknown_command", format!("unknown command {other:?}")),
        }
    }

    fn status(&self) -> Response {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let last_log_seq = self
            .logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_seq();
        Response::ok(json!({
            "daemon_pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "config_path": self.config_path.display().to_string(),
            "last_log_seq": last_log_seq,
            "snapshot": snapshot,
        }))
    }

    fn logs(&self, request: &Request) -> Response {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            after: u64,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_limit() -> usize {
            200
        }
        let args: Args = match request.args() {
            Ok(args) => args,
            Err(response) => return response,
        };
        let logs = self
            .logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lines = logs.after(args.after, args.limit.clamp(1, 2000));
        let last_seq = logs.last_seq();
        Response::ok(json!({ "lines": lines, "last_seq": last_seq }))
    }

    fn wake(&self, request: &Request) -> Response {
        #[derive(Deserialize)]
        struct Args {
            host: String,
        }
        let args: Args = match request.args() {
            Ok(args) => args,
            Err(response) => return response,
        };
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = find_host_index(&snapshot, &args.host) else {
            return Response::error(
                "not_found",
                format!(
                    "host {:?} is not configured; configured hosts: {}",
                    args.host,
                    snapshot
                        .hosts
                        .iter()
                        .map(|host| format!("{} ({})", host.name, host.ip))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        };
        let name = snapshot.hosts[index].name.clone();
        drop(snapshot);
        let sender = self
            .host_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match sender.send(Command::WakeNow(name)) {
            Ok(()) => Response::message(format!("wake packet queued for {:?}", args.host)),
            Err(_) => Response::error("daemon", "the monitor thread is not running"),
        }
    }

    fn pppoe_scan(&self) -> Response {
        let (reply, receiver) = mpsc::channel();
        {
            let sender = self
                .wan_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sender.send(WanCommand::Scan { reply }).is_err() {
                return Response::error("daemon", "the PPPoE watcher is not running");
            }
        }
        match receiver.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(statuses)) => Response::ok(json!({ "interfaces": statuses })),
            Ok(Err(err)) => Response::error("rci", err),
            Err(_) => Response::error("timeout", "the PPPoE watcher did not answer in time"),
        }
    }

    fn pppoe_restart(&self, request: &Request) -> Response {
        #[derive(Deserialize)]
        struct Args {
            interface: String,
            #[serde(default)]
            force: bool,
            #[serde(default)]
            test: bool,
            #[serde(default)]
            dry_run: bool,
        }
        let args: Args = match request.args() {
            Ok(args) => args,
            Err(response) => return response,
        };
        let (reply, receiver) = mpsc::channel();
        {
            let sender = self
                .wan_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sender
                .send(WanCommand::Restart {
                    interface: args.interface.clone(),
                    force: args.force,
                    oneshot: args.test,
                    dry_run: args.dry_run,
                    reply,
                })
                .is_err()
            {
                return Response::error("daemon", "the PPPoE watcher is not running");
            }
        }
        match receiver.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(message)) => Response::message(message),
            Ok(Err(err)) => Response::error("pppoe", err),
            Err(_) => Response::error("timeout", "the PPPoE watcher did not answer in time"),
        }
    }

    fn reload(&self) -> Response {
        let new_config = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(err) => return Response::error("config", err),
        };
        let old_config = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let (host_reply, host_rx) = mpsc::channel();
        let (wan_reply, wan_rx) = mpsc::channel();
        {
            let sender = self
                .host_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sender
                .send(Command::Reload {
                    config: Box::new(new_config.clone()),
                    reply: host_reply,
                })
                .is_err()
            {
                return Response::error("daemon", "the monitor thread is not running");
            }
        }
        {
            let sender = self
                .wan_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sender
                .send(WanCommand::Reload {
                    config: Box::new(new_config.clone()),
                    reply: wan_reply,
                })
                .is_err()
            {
                return Response::error("daemon", "the PPPoE watcher is not running");
            }
        }

        match host_rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Response::error("config", format!("host monitoring reload failed: {err}"))
            }
            Err(_) => {
                return Response::error("timeout", "the monitor did not apply the reload in time")
            }
        }
        match wan_rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Response::error("config", format!("PPPoE reload failed: {err}"))
            }
            Err(_) => {
                return Response::error(
                    "timeout",
                    "the PPPoE watcher did not apply the reload in time",
                )
            }
        }

        *self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_config.clone();
        let restart_required = restart_required(&old_config, &new_config);
        Response::ok(json!({
            "applied": ["hosts", "wol", "pppoe", "rci"],
            "restart_required": restart_required,
        }))
    }
}

fn find_host_index(snapshot: &crate::model::Snapshot, selector: &str) -> Option<usize> {
    let selector = selector.trim();
    if let Some(index) = snapshot
        .hosts
        .iter()
        .position(|host| host.name.eq_ignore_ascii_case(selector))
    {
        return Some(index);
    }
    if let Ok(address) = selector.parse::<std::net::Ipv4Addr>() {
        if let Some(index) = snapshot.hosts.iter().position(|host| host.ip == address) {
            return Some(index);
        }
    }
    if let Ok(index) = selector.parse::<usize>() {
        if index >= 1 && index <= snapshot.hosts.len() {
            return Some(index - 1);
        }
    }
    None
}

fn restart_required(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.log_file != new.log_file {
        fields.push("log_file");
    }
    if old.pid_file != new.pid_file {
        fields.push("pid_file");
    }
    if old.control_socket != new.control_socket {
        fields.push("control_socket");
    }
    if old.init_script != new.init_script {
        fields.push("init_script");
    }
    fields
}

pub struct ServerHandle {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

pub fn spawn_server(path: &Path, control: Arc<Control>) -> Result<ServerHandle, String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create socket directory {}: {err}", parent.display()))?;
    }
    if path.exists() {
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(format!(
                    "control socket {} is already in use by a running daemon",
                    path.display()
                ))
            }
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let listener = UnixListener::bind(path)
        .map_err(|err| format!("cannot bind control socket {}: {err}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("cannot configure control socket {}: {err}", path.display()))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let socket = path.to_path_buf();
    let join = thread::Builder::new()
        .name("control".to_string())
        .spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let control = control.clone();
                        let _ = thread::Builder::new()
                            .name("control-client".to_string())
                            .spawn(move || handle_connection(stream, &control));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        })
        .map_err(|err| format!("cannot spawn the control thread: {err}"))?;

    Ok(ServerHandle {
        socket,
        stop,
        join: Some(join),
    })
}

fn handle_connection(mut stream: UnixStream, control: &Control) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let read_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) if line.len() > MAX_REQUEST => {
            let _ = write_response(
                &mut stream,
                &Response::error("bad_request", "request is too large"),
            );
            return;
        }
        Ok(_) => {}
        Err(_) => return,
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(err) => {
            let _ = write_response(
                &mut stream,
                &Response::error("bad_request", format!("invalid JSON: {err}")),
            );
            return;
        }
    };
    let response = if request.proto > PROTO_VERSION {
        Response::error(
            "proto",
            format!(
                "request protocol {} is newer than {}",
                request.proto, PROTO_VERSION
            ),
        )
    } else {
        control.handle(&request)
    };
    let _ = write_response(&mut stream, &response);
}

fn write_response(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    let text = serde_json::to_string(response).unwrap_or_else(|_| {
        "{\"ok\":false,\"error\":\"cannot serialize response\",\"kind\":\"internal\"}".to_string()
    });
    let text: String = if text.len() > MAX_RESPONSE {
        "{\"ok\":false,\"error\":\"response is too large\",\"kind\":\"internal\"}".to_string()
    } else {
        text
    };
    stream.write_all(text.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[derive(Clone)]
pub struct ControlClient {
    path: PathBuf,
    read_timeout: Duration,
}

impl ControlClient {
    pub fn connect(path: impl Into<PathBuf>) -> Result<ControlClient, String> {
        Ok(ControlClient {
            path: path.into(),
            read_timeout: Duration::from_secs(25),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> ControlClient {
        self.read_timeout = timeout;
        self
    }

    pub fn request(&self, request: &Request) -> Result<Response, String> {
        let mut stream = UnixStream::connect(&self.path).map_err(|err| {
            format!(
                "cannot connect to wolguard control socket {}: {err}",
                self.path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(self.read_timeout))
            .map_err(|err| err.to_string())?;
        stream
            .set_write_timeout(Some(self.read_timeout))
            .map_err(|err| err.to_string())?;
        let text = serde_json::to_string(request).map_err(|err| err.to_string())?;
        stream
            .write_all(text.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .map_err(|err| err.to_string())?;
        stream.flush().map_err(|err| err.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|err| format!("cannot read the daemon response: {err}"))?;
        if line.is_empty() {
            return Err("the daemon closed the control socket without a response".to_string());
        }
        serde_json::from_str(&line).map_err(|err| format!("invalid daemon response: {err}"))
    }

    pub fn status(&self) -> Result<Value, String> {
        let response = self.request(&Request::new("status"))?;
        response.data.ok_or_else(|| {
            response
                .error
                .unwrap_or_else(|| "empty status response".to_string())
        })
    }

    pub fn ping(&self) -> Result<(), String> {
        let response = self.request(&Request::new("ping"))?;
        if response.ok {
            Ok(())
        } else {
            Err(response.error.unwrap_or_else(|| "ping failed".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostState, HostStatus, LogBuffer, Snapshot};
    use std::net::Ipv4Addr;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_socket(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "wolguard-control-{}-{}-{name}.sock",
            std::process::id(),
            nanos
        ))
    }

    fn test_control() -> (
        Arc<Control>,
        mpsc::Receiver<Command>,
        mpsc::Receiver<WanCommand>,
    ) {
        test_control_at(None)
    }

    fn test_control_at(
        path: Option<&Path>,
    ) -> (
        Arc<Control>,
        mpsc::Receiver<Command>,
        mpsc::Receiver<WanCommand>,
    ) {
        let snapshot = Arc::new(Mutex::new(Snapshot {
            hosts: vec![HostStatus {
                name: "server".to_string(),
                ip: Ipv4Addr::new(192, 168, 1, 10),
                mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
                enabled: true,
                state: HostState::Online,
                consecutive_failures: 0,
                last_rtt: None,
                last_ok: None,
                last_check: None,
                last_wake: None,
                wake_count: 0,
            }],
            started_at: UNIX_EPOCH,
            config_path: "/tmp/test.conf".to_string(),
            wol_port: 9,
            wake_target: "none".to_string(),
            last_update: UNIX_EPOCH,
            wol_enabled: true,
            wan: Vec::new(),
            session_signals: Vec::new(),
        }));
        let logs = Arc::new(Mutex::new(LogBuffer::new(10)));
        let (host_tx, host_rx) = mpsc::channel();
        let (wan_tx, wan_rx) = mpsc::channel();
        let config = Config::from_toml_str(
            r#"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
"#,
        )
        .unwrap();
        let mut control = Control::new(snapshot, logs, host_tx, wan_tx, config);
        if let Some(path) = path {
            control = control.with_config_path(path.to_path_buf());
        }
        (Arc::new(control), host_rx, wan_rx)
    }

    #[test]
    fn response_round_trips() {
        let response = Response::ok(json!({"hello": "world"}));
        let text = serde_json::to_string(&response).unwrap();
        let decoded: Response = serde_json::from_str(&text).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.data.unwrap()["hello"], "world");
    }

    #[test]
    fn ping_status_and_logs_over_the_socket() {
        let (control, _host_rx, _wan_rx) = test_control();
        let path = temp_socket("ping");
        let handle = spawn_server(&path, control).unwrap();
        let client = ControlClient::connect(&path).unwrap();
        client.ping().unwrap();
        let status = client.status().unwrap();
        assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
        let logs = client
            .request(&Request::with_args(
                "logs",
                json!({"after": 0, "limit": 10}),
            ))
            .unwrap();
        assert!(logs.ok);
        drop(handle);
        assert!(!path.exists());
    }

    #[test]
    fn wake_is_forwarded_to_the_monitor() {
        let (control, host_rx, _wan_rx) = test_control();
        let path = temp_socket("wake");
        let handle = spawn_server(&path, control).unwrap();
        let client = ControlClient::connect(&path).unwrap();
        let response = client
            .request(&Request::with_args("wake", json!({"host": "server"})))
            .unwrap();
        assert!(response.ok, "{:?}", response.error);
        match host_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            Command::WakeNow(name) => assert_eq!(name, "server"),
            other => panic!("unexpected command: {other:?}"),
        }
        drop(handle);
    }

    #[test]
    fn unknown_command_is_an_error() {
        let (control, _host_rx, _wan_rx) = test_control();
        let response = control.handle(&Request::new("nope"));
        assert!(!response.ok);
        assert_eq!(response.kind.as_deref(), Some("unknown_command"));
    }

    #[test]
    fn reload_is_applied_by_both_threads_and_reports_restart_fields() {
        const SAMPLE: &str = r#"
log_file = "/tmp/wolguard-reload-1.log"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
"#;
        let config_path = temp_socket("reload-conf").with_extension("conf");
        std::fs::write(&config_path, SAMPLE).unwrap();
        let (control, host_rx, wan_rx) = test_control_at(Some(&config_path));
        let host = thread::spawn(move || {
            while let Ok(command) = host_rx.recv() {
                match command {
                    Command::Reload { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    Command::Shutdown => break,
                    _ => {}
                }
            }
        });
        let wan = thread::spawn(move || {
            while let Ok(command) = wan_rx.recv() {
                match command {
                    WanCommand::Reload { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    WanCommand::Shutdown => break,
                    _ => {}
                }
            }
        });
        let socket = temp_socket("reload");
        let handle = spawn_server(&socket, control.clone()).unwrap();
        let client = ControlClient::connect(&socket).unwrap();

        let response = client.request(&Request::new("reload")).unwrap();
        assert!(response.ok, "{:?}", response.error);

        let changed = SAMPLE.replace("wolguard-reload-1.log", "wolguard-reload-2.log");
        std::fs::write(&config_path, changed).unwrap();
        let response = client.request(&Request::new("reload")).unwrap();
        assert!(response.ok, "{:?}", response.error);
        let restart = response
            .data
            .as_ref()
            .and_then(|data| data.get("restart_required"))
            .and_then(|value| value.as_array())
            .expect("restart_required array");
        assert!(
            restart.iter().any(|value| value == "log_file"),
            "{restart:?}"
        );

        drop(handle);
        drop(control);
        let _ = host.join();
        let _ = wan.join();
        let _ = std::fs::remove_file(&config_path);
    }
}
