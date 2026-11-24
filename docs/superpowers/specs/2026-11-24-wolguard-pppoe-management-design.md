# wolguard 0.2 — PPPoE white-IP watchdog and management console

Status: implemented in wolguard 0.2.0
Date: 2026-11-24
Repo: keenetic-wolguard

Implementation notes (differences from the first draft, requested during
review):

- `on-change` hunts immediately on the first observation after daemon start or
  target enable when the address is grey, then on later session changes.
- `pppoe test` is a one-shot: a grey result does not re-arm the automatic hunt;
  the next session change does.
- State-file observations are diagnostic; they do not suppress the first hunt
  after a daemon restart.
- Automatic RCI token bootstrap is implemented in the daemon as well as in
  `wolguard rci token`.

## 1. Intent

`wolguard` currently watches LAN hosts with ICMP/Wake-on-LAN and offers a
read-only ratatui dashboard. Two things are missing for the real setup it runs
on:

1. The router can reconnect PPPoE and receive a *grey* IPv4 address (private or
   CGNAT). The useful address is a *white* (globally routable) one. The user
   wants the daemon to notice a PPPoE reconnect with a grey address and keep
   restarting the PPPoE session until a white address appears, within sane
   rules.
2. The utility is configured by hand-editing TOML and its CLI is a thin set of
   one-shot commands. The user wants the whole thing — hosts, PPPoE target,
   rules, global settings — configured comfortably from a TUI/CLI, with the
   config written safely behind the scenes.

The work is a project-sized rework, not a small feature.

## 2. Goals

- Discover PPPoE interfaces on a Keenetic router and show their current IPv4,
  state, uptime and white/grey classification.
- Let the user select one or more PPPoE targets to watch and configure rules for
  when and how often to restart them.
- Watch the selected interfaces; when a session reconnects with a grey address,
  restart PPPoE (`up=false` / `up=true` via RCI), wait for a new address, and
  repeat until the address is white or the rate limit is reached.
- Make trigger policy configurable:
  - `on-change` (default): evaluate immediately when the daemon starts or a
    target is enabled, and start a hunt if the address is grey; afterwards
    start a new hunt on a PPPoE reconnect / address change. It does not start
    a new hunt on every poll for a stable grey address.
  - `always`: start a hunt whenever the current address is grey, including a
    stable grey address that did not change.
- Make hosts, PPPoE targets, rules and global settings editable from the TUI
  and from non-interactive CLI commands. No hand-editing of TOML needed for
  normal operation.
- Turn the TUI into a client of the running daemon over a local control
  socket, so it can show live state and act while the service is running.
- Keep the existing WOL behaviour, old configs and existing commands working.
- Guarantee that a PPPoE interface this daemon took down is brought back up:
  persistent marker, startup reconciliation, shutdown recovery, and an
  unconditional recovery path that bypasses the hourly limit.
- Allow a PPPoE-only deployment: a config with zero `[[host]]` entries is valid
  when at least one PPPoE target is configured.
- Bootstrap the Keenetic RCI access token automatically through `ndmc` when the
  firmware requires one, so the user does not have to paste a token by hand.
- Make the CLI pleasant: consistent tables, colours when the terminal supports
  them, interactive pickers on a TTY, useful help and error messages, and
  plain output for the Keenetic web console (no PTY).
- Update README, default config, help text and troubleshooting.

## 3. Non-goals

- PPTP/L2TP/other WAN types. The model is extensible, but 0.2 targets PPPoE.
- Managing VPN tunnels (WireGuard/OpenVPN) or third-party services.
- A web UI.
- TLS or remote RCI. RCI is localhost HTTP; an optional `X-NDMA-TKN` token is
  supported.
- A general-purpose TOML configuration framework.
- Automatic ISP account/credential rotation. Only interface up/down is used.
- Supporting routers without RCI for PPPoE control. `ndmc` is used only for
  token bootstrap and diagnostics, not as the normal PPPoE backend.

## 4. Anti-goals

- Breaking a 0.1.1 config. Every new field is optional with a default.
- Requiring `curl`, `jq`, `ndmq` or any other runtime binary on the router for
  normal operation.
- Losing comments or unrelated settings when the CLI/TUI writes the config.
- Blocking LAN host monitoring while a PPPoE restart waits for a new address.
- Leaving the PPPoE interface down if the daemon crashes or is stopped
  mid-restart.
- Restarting PPPoE without limits or spamming the ISP.
- Destroying per-target rules when the user unwatches an interface.
- Refusing to save a PPPoE-only config because no LAN hosts are configured.
- Holding the instance lock in the TUI, which currently prevents using it while
  the daemon runs.

## 5. Constraints and verified facts

- Target: Keenetic Giga KN-1011, MIPS32r2, soft-float, musl static binary.
  No async runtime; use blocking threads.
- The router has limited RAM/flash; dependencies and binary size matter.
- A KeeneticOS 5.1.6 capture confirms the RCI read shape:
  `GET /rci/show/interface/` returns an object keyed by interface id, with
  `type: "PPPoE"`, `connected: "yes"` as a string, `uptime` as integer
  seconds, `description` as the provider name, and no `summary` block on
  PPPoE. The implementation treats `summary` as optional.
- The exact RCI write shape for down/up is not verified on the target
  firmware. The design therefore probes the documented forms, verifies each
  by readback, caches the working form, and reports an actionable error if none
  works. It never assumes a 200 response means the interface changed.
- RCI on `127.0.0.1:79`. KeeneticOS 5.2+ may require an access token
  (`X-NDMA-TKN`); the daemon can generate one via `ndmc` and store it.
- The TUI runs over SSH; `/dev/tty` may not implement `TIOCGWINSZ`; terminals
  can be 40 columns wide. Existing terminal handling must be preserved.
- Root is available on the router; the control socket and raw ICMP require it.
- The Keenetic web console has no PTY; all commands used there must work
  without raw mode.

## 6. Approaches considered

1. **Integrated Rust implementation using RCI** (chosen).
   A small HTTP client talks to RCI; PPPoE discovery, classification and the
   restart state machine live in the daemon. The TUI/CLI become clients of the
   daemon. One binary, one service, one log, no runtime dependencies.
2. **`ndmc` text backend** (fallback, not chosen).
   Same UX, but reads `show interface` text and sends `interface <id>` +
   `down`/`up`. Avoids RCI tokens but is brittle across firmware and hard to
   test. Kept as a documented diagnostic path only.
3. **Separate shell daemon with curl/jq and cron** (rejected).
   Two services, external dependencies, no unified dashboard or logs. It does
   not satisfy "improve the utility".

The chosen approach requires a control socket because the current TUI runs its
own monitor and takes the instance lock. Without a socket, configuration
through the TUI while the service runs is impossible.

## 7. Architecture

### 7.1 Processes and commands

```
wolguard run                daemon: monitor + WAN watcher + control server
wolguard (no args)          TUI client on a TTY, status otherwise
wolguard tui                TUI client
wolguard tui --standalone   development mode: in-process monitor, no daemon
wolguard status             daemon snapshot if available, else live probe
wolguard pppoe list/scan    direct RCI scan, or daemon scan when connected
wolguard pppoe restart      through the daemon only (safe recovery)
wolguard pppoe watch        writes a target block via configedit
wolguard hosts ...          list/add/edit/remove hosts via configedit
wolguard setup              interactive first-run wizard
wolguard settings ...       show/set global settings
wolguard doctor             environment/config/RCI diagnostics
```

The daemon owns the instance lock, PID file and control socket. The TUI and CLI
are clients; they never spawn a second monitor while a daemon is alive.
Duplication protection is layered: an exclusive `flock` on a lock file, the
PID file check, and the control-socket bind. A second `wolguard run` prints the
running daemon's status and exits 0 instead of starting another monitor;
`tui --standalone` refuses to start when a daemon is alive.

If the daemon is not running:

- `status`, `pppoe list`, `pppoe show`, `hosts`, config commands and `wake`
  still work directly (current behaviour where applicable);
- `pppoe restart` refuses: a one-shot CLI process can die with the SSH session
  after `up=false` and leave the WAN down. The message tells the user to start
  the service and retry;
- `tui` shows a banner and offers to start the service;
- `tui --standalone` preserves the development workflow.

### 7.2 Threads inside the daemon

```
main
 ├── monitor thread      host pings, wake policy, host state
 ├── wan thread          PPPoE scan, classification, restart state machine
 ├── control thread      Unix socket accept loop, request handlers
 └── signal/lifecycle    stop flag, state-file reconciliation, cleanup
```

- The monitor and WAN threads never wait on each other.
- Both update a single `SharedSnapshot` under a short mutex.
- Both write through the existing `Logger`/`LogBuffer`.
- The control thread clones snapshots and sends commands through per-thread
  `mpsc` channels. It never blocks monitoring.
- On shutdown, the WAN thread interrupts its waits, issues `up=true`
  immediately for an interface it owns, retries for a few seconds, clears the
  persistent marker, and exits. It does not wait for `settle_secs` while
  shutting down. The init script's stop timeout is raised from 10 s to 15 s to
  leave room for this.
- If the process is SIGKILLed or aborts between `down` and `up`, the persistent
  marker remains and the next daemon start restores the interface before any
  monitoring.

### 7.3 Control socket protocol

- Default path: `/tmp/wolguard.sock` (tmpfs; some Entware USB volumes are vfat
  and do not support Unix sockets), configurable as `control_socket`. Unix
  `SOCK_STREAM`, permissions `0600`, removed on clean exit; a stale socket is
  detected by a failed connect and unlinked only when no daemon is alive.
- One request per connection: client connects, writes one JSON line, reads one
  JSON line, closes. The TUI polls at ~1 s.
- Requests:

```json
{"proto":1,"cmd":"status"}
{"proto":1,"cmd":"logs","after":42,"limit":200}
{"proto":1,"cmd":"wake","host":"server"}
{"proto":1,"cmd":"pppoe-scan"}
{"proto":1,"cmd":"pppoe-restart","interface":"PPPoE0","force":false}
{"proto":1,"cmd":"reload"}
{"proto":1,"cmd":"ping"}
```

- Responses:

```json
{"ok":true,"proto":1,"data":{...}}
{"ok":false,"proto":1,"error":"host not found","kind":"not_found"}
```

- `status` data: the full `Snapshot` (hosts, WAN interfaces, uptime, config
  path, wake target, daemon pid/version, last log sequence, available
  session-change signals) plus `daemon_pid` and `version`.
- `logs`: `after` is a monotonically increasing per-line sequence; only newer
  lines are returned. `LogLine` gains `seq: u64`. The TUI keeps the last seen
  sequence and resets it when the daemon pid changes or the sequence goes
  backwards.
- `reload`: the daemon re-reads the config file, validates it and applies it.
  Response data: `{applied: [...], restart_required: [...]}`.
- `pppoe-restart` returns after the cycle is accepted. `force` bypasses the
  hourly limit but still logs the override. The interface need not be a
  configured target; unwatched interfaces use the global rules and a one-off
  cycle.
- `pppoe-scan` performs a fresh RCI scan and returns the interface list. It
  works even when `[pppoe] enabled = false`.
- `shutdown` is not exposed through the socket in 0.2; service stop uses the
  init script and signals.

### 7.4 RCI client

New module `rci.rs`:

- Minimal HTTP/1.1 over `std::net::TcpStream`, no TLS.
- Base URL `http://127.0.0.1:79` by default; parse `host:port` and
  `http://host:port`.
- Optional `X-NDMA-TKN` header from `rci.token`.
- Methods:
  - `list_interfaces()` → `GET /rci/show/interface/`
  - `set_interface_up(id, up)` → probe write forms, verify by readback
  - `dhcp_bindings()` → `GET /rci/show/ip/dhcp/bindings` (setup wizard)
  - `version()` → `GET /rci/show/version` (doctor)
- HTTP handling: connect/read timeouts, `Content-Length` and chunked bodies,
  a 1 MiB body cap, status-line/header parsing. Errors carry the HTTP status
  and a short body snippet.
- 401/403 maps to a dedicated `Unauthorized` error.

Verified read shape (KeeneticOS 5.1.6):

- Response is a JSON object keyed by interface id.
- Fields used: `id`, `interface-name`, `type`, `description`, `state`,
  `link`, `connected` (string `"yes"`/`"no"`), `address`, `mask`, `uptime`
  (integer seconds), `priority`, and `summary` when present.
- `address`/`mask` are strings; empty or `0.0.0.0` means no address.
- `summary` may be absent (PPPoE); `summary.layer.ipv4 == "running"` is
  required only when the block is present.
- `last-change` is parsed best-effort when present.

Write forms (probed in this order, each verified by re-reading the interface):

1. `POST /rci/interface/<id>` with `{"up": "true"|"false"}` — the string form
   used by kvas on Keenetic.
2. `POST /rci/interface/<id>` with `{"up": true|false}` — the boolean form.
3. `POST /rci/interface` with `{"<id>": {"up": true|false}}` — the documented
   id-keyed interface example.
4. `POST /rci/interface` with the same payload using a string flag.
5. `POST /rci/` with `{"interface": {"<id>": {"up": true|false}}}` — the
   nested config-tree form, boolean.
6. `POST /rci/` with the same payload using a string flag.

Rules:

- A form is accepted only if the readback shows the expected change: for down,
  the interface is no longer up/connected; for up, it is up/connected with an
  address (waiting a few seconds for PPPoE to settle).
- The working form is cached for the process lifetime. If all forms fail, the
  error names the attempts and tells the user to run `wolguard doctor`.
- The daemon never sends `up=true` after a `down` that did not take effect;
  there is nothing to restore, and the cycle is logged as a failed actuator
  rather than counted as a restart.
- The recovery path retries `up=true` with the cached/working form and
  bypasses the hourly limit.

Token bootstrap:

- On a 401/403, if `[rci] auto_token = true` (default) and `ndmc` is available:
  - run `ndmc -c "authentication token generate wolguard"`;
  - parse the `value:` line from the output;
  - save the token through `configedit` (atomic, file mode 0600) and run
    `ndmc -c "system configuration save"`;
  - retry the request once and log the recovery.
- `ndmc_path` defaults to `ndmc` and, if not on `PATH`, tries `/bin/ndmc`,
  `/usr/sbin/ndmc`, `/opt/bin/ndmc`.
- `wolguard rci token` forces regeneration; `wolguard doctor` reports the RCI
  auth mode and the exact next step if automatic bootstrap is unavailable.
- `ndmc` is used only for token bootstrap and diagnostics, never as the normal
  PPPoE control path.

### 7.5 PPPoE model and state machine

New module `pppoe.rs`.

Discovery:

- One `GET /rci/show/interface/` per poll returns all interfaces.
- Keep entries with `type == "PPPoE"`. Sort by `id`.
- An entry is *usable* when `state == "up"` and `link == "up"` and
  `connected == "yes"` and a parsed address other than `0.0.0.0` exists. If a
  `summary` block is present, `summary.layer.ipv4 == "running"` is also
  required.
- Only configured targets are acted on automatically; all PPPoE entries are
  displayed.

Classification (`IpClass`):

1. Address matches any `grey_cidrs` → grey (explicit deny wins).
2. Address matches any `white_cidrs` → white (explicit allow, can override the
   built-in special-use table).
3. Built-in special-use check → grey.
4. Otherwise → white.

Built-in grey ranges: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`,
`169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.0.2.0/24`,
`192.88.99.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `198.51.100.0/24`,
`203.0.113.0/24`, `224.0.0.0/4`, `240.0.0.0/4`, `255.255.255.255/32`.
Every address gets a human-readable reason ("public", "private 10/8", "CGNAT
100.64/10", "matches grey_cidrs 203.0.113.0/24", etc.) for logs and UI.

Session-change signals (for `on-change`):

- address changed;
- `uptime` decreased (session reset);
- a non-usable state was observed since the last usable observation
  (down→up transition seen by polling);
- `last-change` changed, when the firmware exposes it.

The daemon records these signals and reports in `status` which are available
(`session_signals: ["address","uptime",...]`). If neither `uptime` nor
`last-change` resets on a same-address re-dial, `on-change` can only see
address changes; `doctor` reports this, and the docs recommend `always` for
that case. After a restart initiated by the daemon, the hunt is already armed,
so it does not depend on signal detection.

Per-target runtime state:

```
last_address: Option<Ipv4Addr>
last_uptime: Option<u64>
last_change: Option<String>
last_usable: bool
hunting: bool
attempts: VecDeque<Instant>   // sliding 1-hour window, not cleared on success
next_action: Option<Instant>
last_restart: Option<SystemTime>
last_error: Option<String>
```

Tick loop (default 15 s poll):

1. If the interface is missing or not usable → record state, do not restart;
   if hunting, keep hunting armed but wait. If this daemon owns a pending
   down for it, the recovery path takes precedence.
2. If an address is present:
   - Determine `session_changed` from the signals above.
   - White → if hunting, log success and stop; reset the hunt. Update the
     observation. The hourly attempt window is not cleared.
   - Grey:
     - `trigger=always`: start hunting.
     - `trigger=on-change`: start hunting on the first observation after daemon
       start or target enable when the address is grey, and afterwards when
       `session_changed`. A stable grey address is not re-hunted on every poll.
     - If already hunting, continue.
3. Hunt cycle, subject to the hourly limit and `retry_secs`:
   - Verify the interface is still available and usable.
   - Write the persistent marker (`state_file`) with the interface id and time.
   - `set_interface_up(id, false)` with readback verification; wait
     `down_secs` (interruptible, stop-aware).
   - `set_interface_up(id, true)` with readback verification; poll every 2 s
     up to `settle_secs` for a usable address.
   - If an address appears: classify. White → success, reset hunt, clear the
     marker. Grey → record attempt, schedule the next cycle after
     `retry_secs`, keep the marker until a verify-up succeeds.
   - If no address in time → record attempt, log, schedule the next cycle,
     keep the marker.
   - If RCI fails → log the error, back off, do not count a successful
     attempt; retry on the next poll. If the down succeeded but the up failed,
     do not wait for the normal schedule: retry up on every tick with a short
     backoff until the interface is usable.
   - Rate limit: sliding 1-hour window of at most `max_restarts_per_hour`.
     When full, log once and set `next_action` to the time the oldest attempt
     expires; resume automatically if still grey and hunting. A manual restart
     counts against the same window unless `force` is set.
4. Log transitions only, not every poll. Example lines:

```
pppoe PPPoE0 (Дом. интернет): 100.70.12.34 is grey (CGNAT 100.64/10); starting hunt
pppoe PPPoE0: restart 1/12 (down)
pppoe PPPoE0: up, waiting for an address (up to 60 s)
pppoe PPPoE0: new address 95.31.12.7 is white (public); hunt complete after 1 restart
pppoe PPPoE0: still grey after 12 restarts; rate limit reached, next attempt 14:05
```

Recovery contract:

- The state file (`state_file`, default `/opt/var/run/wolguard.state`) is
  written atomically before the down POST and cleared only after a verified
  up. It also stores the last observations for diagnostics; observations do not
  suppress the first-observation hunt after a daemon restart.
- At daemon start, only `pending_down` entries recorded by this daemon are
  reconciled before normal monitoring: `up=true` with the cached/working form,
  verify, clear. Interfaces that were already down for other reasons are never
  brought up: they may be intentionally disabled or managed by another policy.
  If recovery fails, retry every tick with backoff, bypassing the hourly
  limit, and log loudly.
- On SIGTERM/SIGINT, the WAN thread interrupts waits, issues `up=true` for any
  owned down, retries for up to ~5 s, clears the marker, and exits. The init
  script waits 25 s before SIGKILL (enough for a slow recovery `up`; the
  persistent marker covers the SIGKILL case).
- On SIGKILL or abort, the marker remains; the next start recovers.
- The "never act on a down interface" rule applies only to interfaces this
  daemon does not own. Owned pending-down interfaces are always restored.

Manual restart contract:

- `pppoe restart <id>` is a single down/up cycle, not a full hunt. It is
  accepted only through the daemon in 0.2. It counts against the target's
  window (or the global window for an unwatched interface) unless `force` is
  set.
- If the interface is a watched target and the new address is grey, the normal
  state machine continues the hunt according to its trigger/limits. If the
  interface is unwatched, the cycle ends after the readback.
- `--force` bypasses the limit but logs "manual override".
- `pppoe test <id> [--dry-run]` is the manual test path: it evaluates the
  selected interface, prints the current address/class, and (unless
  `--dry-run`) performs one restart cycle synchronously, reporting the new
  address/class. It is available from the TUI WAN tab as `t`. A dry run never
  touches the interface.

### 7.6 Configuration

Existing top-level WOL keys stay unchanged. New optional keys:

```toml
# Global feature switches: turn off what you do not use.
wol_enabled = true      # false disables all LAN host monitoring

# Control socket used by the TUI/CLI to talk to the daemon.
control_socket = "/tmp/wolguard.sock"
# Init script used by `wolguard service ...` and the TUI.
init_script = "/opt/etc/init.d/S99wolguard"
# Persistent recovery/observation state (atomic writes).
state_file = "/opt/var/run/wolguard.state"

[rci]
url = "http://127.0.0.1:79"
token = ""              # optional X-NDMA-TKN for KeeneticOS 5.2+
timeout_ms = 2000
auto_token = true       # generate a token via ndmc on 401/403
ndmc_path = "ndmc"      # optional override; common paths are tried

[pppoe]
enabled = true
trigger = "on-change"   # "on-change" | "always"
poll_secs = 15
down_secs = 5
settle_secs = 60
retry_secs = 45
max_restarts_per_hour = 12
white_cidrs = []
grey_cidrs = []

[[pppoe.target]]
interface = "PPPoE0"
# Per-target overrides (all optional):
# enabled = true
# trigger = "always"
# poll_secs = 15
# down_secs = 5
# settle_secs = 60
# retry_secs = 45
# max_restarts_per_hour = 12
# white_cidrs = []
# grey_cidrs = []
```

- `Config` gains `control_socket`, `init_script`, `state_file`, `rci`,
  `pppoe`.
- `PppoeConfig` has global defaults and `targets: Vec<PppoeTargetConfig>`.
  `PppoeTargetConfig` uses `Option<T>` overrides resolved by a
  `pppoe_targets()` helper, analogous to `host_settings()`.
- All new fields are `#[serde(default)]`; a 0.1.1 config parses unchanged.
- `deny_unknown_fields` stays.
- Validation:
  - At least one host **or** at least one PPPoE target must be configured.
    Zero hosts are valid when a target exists; zero targets are valid when a
    host exists; both empty is an error.
  - `wol_enabled = false` disables LAN host monitoring globally; per-host
    `enabled` and `[pppoe] enabled` / per-target `enabled` give finer control.
    The TUI Settings tab exposes the global switches.
  - Non-empty unique target ids, positive intervals, valid CIDRs, valid URL,
    non-empty socket/script/state paths, valid trigger enum.
- `watched` vs `enabled`:
  - `watched` means a `[[pppoe.target]]` block exists for the interface.
  - `enabled` means that block is active.
  - `pppoe watch` creates the block with defaults if absent (and never
    overwrites existing rules), then sets `enabled = true`.
  - `pppoe unwatch` sets `enabled = false`, keeping the block and rules.
  - `pppoe remove` deletes the block after confirmation.
  - The TUI `w` toggles `enabled`; a separate delete action removes the block.

### 7.7 Config editing

New module `configedit.rs`, using `toml_edit` (already in the dependency graph
through `toml 0.8`, so no new transitive tree) so comments, key order and
unrelated content survive:

- `ConfigDoc::load(path)`; create from a built-in template if missing.
- Record a revision (mtime + content hash) at load. Before saving, if the file
  changed underneath, refuse and ask the UI/user to reload or force; this
  prevents silent lost edits between two sessions.
- Operations:
  - `set_global(key, value)` for scalar settings;
  - `upsert_host(HostEdit)`, `remove_host(name)`, `set_host_enabled(name, bool)`;
  - `upsert_pppoe_target(id, rules)`, `set_pppoe_target_enabled(id, bool)`,
    `remove_pppoe_target(id)`;
  - `set_rci(url, token, timeout, auto_token)`, `set_pppoe_defaults(rules)`;
  - `set_control_socket`, `set_init_script`, `set_state_file`;
  - `set_rci_token(token)` (0600 file mode).
- `remove_host` refuses to remove the last host when no PPPoE target exists.
- Before writing, render the document to a string and parse it with
  `Config::from_toml_str`; a validation error aborts the save and is shown in
  the UI.
- Atomic write: temp file in the same directory, `chmod` (preserve existing
  mode, default 0644; 0600 when an RCI token is present), `rename` over the
  original. Keep a single `wolguard.conf.bak` of the previous content.
- Tests: comments/order preserved, repeated edits idempotent, invalid input
  rejected, revision conflict detected, backup created.
- The TUI and CLI never string-format TOML by hand outside this module.

### 7.8 Reload semantics

`reload` re-reads the config and applies what it can:

- Hosts: rebuild the monitor runtime list. Preserve state (`last_ok`,
  counters, wake history) for hosts whose name, IP and MAC are unchanged;
  reset for changed/new hosts; drop removed hosts. Do not re-run
  `wake_on_start` for existing hosts. Zero hosts is valid when a PPPoE target
  exists.
- Global WOL settings (`check_interval_secs`, `ping_timeout_ms`,
  `failures_before_wake`, `wake_retry_secs`, `max_wakes_per_hour`,
  `wake_on_start`, `broadcast`, `wol_port`, `verbose_probes`): apply live.
- PPPoE defaults and targets: apply live; the WAN thread rebuilds its target
  list and preserves runtime state by interface id. Removing a target while it
  is hunting ensures the interface is left up.
- `[rci]`: apply live; the next request uses the new URL/token.
- `state_file`: apply live by saving the state (including any pending-down
  marker) to the new path before switching; the old file is removed after a
  successful move. If the new path cannot be written, the old path is kept and
  an error is logged.
- `log_file`, `pid_file`, `control_socket`, `init_script`: restart-required.
  The response lists them; the TUI shows a notice and offers `service
  restart`.
- If validation or binding fails, the old configuration stays and the response
  is an error.

### 7.9 CLI

Command tree (subject to clap naming; aliases kept):

```
wolguard
  (none)                    TUI on TTY, status otherwise
  run                       daemon
  tui [--standalone]        TUI client
  status [--json]
  setup                     first-run wizard
  hosts list [--json]
  hosts add|edit|remove|enable|disable|wake <name>
  wake <host>               alias of `hosts wake`
  pppoe list|scan [--json]
  pppoe show <id>
  pppoe restart <id> [--force]
  pppoe test <id> [--dry-run] [--force]
  pppoe watch <id>
  pppoe unwatch <id>
  pppoe remove <id> [--yes]
  pppoe rules [<id>]
  settings show|set|edit
  config show|validate|path
  logs [--lines N] [--follow]
  service start|stop|restart|status
  doctor
  rci token                 generate/refresh the RCI access token
  check-config              alias of `config validate`
```

Behaviour:

- `wolguard` with no args is the friendly default: TUI on a TTY; a styled
  `status` otherwise (web console).
- `hosts add` / `edit` are interactive on a TTY (prompted fields with
  defaults) and refuse to run interactively without a TTY; explicit flags
  exist for scripting.
- `hosts add` and `setup` can offer DHCP leases from
  `GET /rci/show/ip/dhcp/bindings` to fill IP/MAC; manual entry remains.
- `pppoe` with no subcommand on a TTY opens a picker:
  `↑/↓`, `r` restart, `w` watch/unwatch, `e` rules, `Enter` details, `q` quit.
  On a non-TTY it prints the table and instructions.
- `pppoe restart` requires the daemon. Over the active WAN it drops the SSH
  session; the daemon survives and restores the interface. The CLI explains
  this before acting; `--force` bypasses the rate limit, not the daemon
  requirement.
- `pppoe list/show/scan` work without the daemon via direct RCI.
- `pppoe unwatch` disables without deleting rules; `pppoe remove` deletes with
  confirmation.
- `--json` prints stable JSON (same snapshot types) and no colour.
- `wake` routes through the daemon when it is running so counters update;
  otherwise it sends directly as today.
- `service` runs the configured init script with fixed arguments; if the script
  is missing, it prints the exact command instead of failing silently.
- `doctor` reports: config path/validity, service/socket status, RCI
  reachability/auth mode/token, discovered PPPoE interfaces and available
  session signals, raw ICMP capability, state-file/pending recovery, log file
  writability, init script presence, and next-step hints.
- If `--config` points the client at a different file than the daemon uses, the
  status response's `config_path` is shown and a warning is printed. The
  control socket path must match for a connection; the warning suggests the
  matching `--config`.

Styling (`style.rs` / `ui.rs`):

- ANSI colour and bold only when stdout is a TTY and `NO_COLOR` is unset,
  `TERM` is not `dumb`, and `CLICOLOR != 0`.
- UTF-8 box drawing when the locale looks UTF-8; `--ascii` /
  `WOLGUARD_ASCII=1` forces ASCII.
- A small table renderer with alignment, width calculation and Unicode-safe
  truncation (reuse the existing care about multi-byte characters).
- Error messages name the next command to try, not just the failed parse.

Example:

```
$ wolguard pppoe
  PPPoE tunnels (Keenetic)

  #   ID        Provider        IPv4            Class   State   Uptime    Watch
  1   PPPoE0    Дом. интернет   100.70.12.34    grey    up      1h 12m    restarting
  2   PPPoE1    Резерв          95.31.12.7      white   up      2d 4h     -

  ↑/↓ select   r restart   w watch   e rules   Enter details   q quit
```

### 7.10 TUI

The TUI becomes a client of the daemon and is organised into tabs:

1. **Overview** — current dashboard: host table, service/WAN summary, recent
   log strip.
2. **Hosts** — host table; `a` add, `e` edit, `d` remove, `w` wake, `Space`
   enable/disable. Modal forms; saving writes the config and reloads.
3. **WAN** — all PPPoE interfaces with ID, provider, IPv4, class, state,
   uptime, attempts, next action, watch status. `r` restart (confirm), `w`
   toggle watch enabled, `d` remove target block (confirm), `e` rules (modal),
   `s` rescan. A pending-down/recovery banner is shown if the state file has an
   owned interface.
4. **Settings** — grouped settings (WOL, RCI, PPPoE defaults, service). Rows
   are editable inline or via a form; `s` saves and reloads; restart-required
   changes show a banner with `S` to run the service restart.
5. **Logs** — scrollable, `f` follow, `i/w/e/a` level filter, `g/G` top/bottom.

- Tab switching: `Tab`/`Shift-Tab` or `1`–`5`; the footer shows the keys for the
  active tab.
- If the daemon is unreachable: a banner "wolguard service is not running";
  `S` starts it, `R` reconnects. Editing config still works.
- After `S` restarts the service, the TUI waits for the socket and reconnects;
  it keeps the last snapshot with a "reconnecting" banner meanwhile.
- The TUI no longer takes the instance lock in client mode. `--standalone`
  keeps the old in-process mode for development.
- Terminal setup/restore, panic hook, `RouterBackend` and narrow-terminal
  handling are reused. Modals and tables must render at 40x10 without panics.
- All user-controlled strings (host names, interface descriptions) are
  sanitised for control characters before rendering to avoid terminal escape
  injection.

### 7.11 Snapshot and concurrency

- `Snapshot` gains `wan: Vec<WanStatus>` and `last_log_seq: u64`, plus
  `session_signals`; derive `Serialize`/`Deserialize` for the protocol types.
- `WanStatus` fields: `id`, `description`, `interface_name`, `state`, `link`,
  `connected`, `address`, `mask`, `class` + `class_reason`, `uptime_secs`,
  `watched`, `enabled`, `hunting`, `restart_count`, `last_restart`,
  `next_action`, `last_error`, `recovery_pending`.
- The monitor and WAN threads update their sections under short locks; the
  control thread clones the snapshot for a response. No lock is held during
  RCI or logging I/O.
- `Command` becomes non-`Copy` to carry reload acknowledgements; host wake
  commands stay simple.

## 8. Error handling and edge cases

- **RCI unreachable**: log once per state change, show `last_error` in status,
  keep host monitoring alive. `doctor` explains the fix.
- **RCI 401/403**: try the token bootstrap; if it fails, `Unauthorized` with
  the exact next step; no restart loop.
- **RCI write form no-ops**: probe the next form; readback after every write;
  if none works, a failed-actuator error, no false attempts, `doctor` hint.
- **Down succeeded, up failed**: marker stays; retry up every tick with
  backoff, bypassing the limit, until usable; log loudly.
- **SIGKILL/abort between down and up**: marker survives; startup reconciles
  before monitoring.
- **Shutdown mid-cycle**: interruptible waits; issue up immediately; clear the
  marker; the init script allows 25 s before SIGKILL.
- **Same-address same-uptime reconnect**: `on-change` may not see it; report
  available signals in `doctor`; docs recommend `always` for that firmware.
- **Interface renamed/removed**: targets are matched by current id; a missing
  target is reported, not recreated. The UI offers rescan/select.
- **Not a PPPoE interface**: target validation at runtime; refuse to act.
- **Grey while interface is down and not owned**: wait; do not restart a dead
  link.
- **Same grey address after restart**: counts as an attempt; retry within the
  limit.
- **Address pending**: wait for a usable address before classifying.
- **Rate limit exhausted**: wait for the oldest attempt to leave the window;
  resume automatically if still grey and hunting.
- **Manual restart over SSH without the daemon**: refused; instructions to
  start the service. Direct `list/scan/show` still work.
- **Config reload invalid**: old config remains; error shown; no partial apply.
- **Concurrent config edits**: revision check; refuse a save over a changed
  file unless forced. Single-admin assumption; atomic writes prevent partial
  reads.
- **Daemon/client config mismatch**: status shows the daemon path; a warning
  suggests the matching `--config`.
- **Log sequence reset**: TUI resets `after` when the daemon pid changes or the
  sequence goes backwards.
- **Zero hosts**: valid with a PPPoE target; monitor idles; UI shows an "add a
  host" hint; `hosts remove` refuses to remove the last host without a target.
- **Two daemons**: instance lock; stale socket is detected by connect failure
  and removed only when no daemon is alive.
- **TUI disconnect**: keep the last snapshot, show a banner, retry every few
  seconds; never panic.
- **Terminal too small / resize / Unicode**: existing layout guarantees and
  tests extended to all tabs and modals.
- **Non-UTF-8 locale/config**: config is read as UTF-8; a parse error names the
  file and line. CLI tables fall back to ASCII.
- **Log file unwritable**: existing behaviour (warning, stdout/ring buffer);
  the control socket keeps working.
- **Host deleted while offline/waking**: drop runtime state safely.
- **Clock changes**: use `Instant` for scheduling and `SystemTime` for display,
  as the existing code does.

## 9. Testing

Unit tests:

- `ipclass`: all special-use ranges, boundaries, CIDR matching, explicit
  white/grey precedence, invalid CIDR.
- `rci`: response parser (status line, headers, content-length, chunked),
  interface JSON parsing (5.1.6 shape, missing summary, string `connected`,
  empty address), error mapping, write-form probing/readback/caching with a
  fake server.
- `pppoe`: state machine with a fake clock/router:
  - on-change startup/enable with grey starts a hunt;
  - a session change to grey starts a hunt;
  - white address stops the hunt;
  - restart → grey → retry → white;
  - no address after restart;
  - rate limit and automatic resume;
  - interface down during a hunt;
  - down succeeded/up failed → recovery retries bypass the limit;
  - shutdown leaves the interface up;
  - state-file marker survives a simulated crash and is reconciled at startup;
  - same-address reconnect with uptime reset/down-up/last-change signals.
- `config`: old 0.1.1 config parses; new fields/defaults; invalid values;
  per-target resolution; zero hosts with a PPPoE target is valid; empty config
  is rejected.
- `configedit`: comments/order preserved; add/edit/remove host and target;
  watch/unwatch/remove semantics; validation failure does not write; revision
  conflict; atomic save/backup; 0600 with a token.
- `control`: request/response serialisation; unknown command; malformed JSON;
  log sequence.
- `cli`: table formatting, colour suppression in non-TTY, JSON output,
  host/target selectors, error hints, restart refusal without a daemon.
- `tui`: render every tab and modal at 40x10, 80x24, 120x40; grey/white states;
  daemon-disconnected banner; recovery banner.

Integration-ish tests without a router:

- A `TcpListener` on `127.0.0.1:0` serving canned RCI JSON, including
  chunked and error responses, and rejecting the first two write forms to test
  fallback.
- A `UnixListener` for the control protocol.
- No tests require root, raw sockets, or a real Keenetic.

Manual device test plan (documented in README):

1. Install, keep the old config; `wolguard status` still works.
2. `wolguard doctor` shows RCI reachable and lists PPPoE.
3. `wolguard pppoe` lists the interfaces; classification matches reality.
4. `wolguard service start`; `wolguard pppoe restart PPPoE0 --force`
   reconnects (SSH drops; reconnect). Verify the interface comes back up and
   the daemon logs the recovery.
5. Watch a target with `trigger=always`; force a grey classification by
   temporarily adding the current public `/32` to `grey_cidrs`; observe the
   restart loop and the success log; remove the temporary CIDR.
6. Edit a host and PPPoE rules in the TUI; verify comments survive and the
   daemon reloads without a restart.
7. Stop/restart the service mid-cycle (SIGTERM and SIGKILL); verify the
   interface is restored and the state file is reconciled.
8. Narrow SSH terminal (40 columns): all tabs and modals remain usable.
9. Optional: on KeeneticOS 5.2+, remove `rci.token`, restart, verify the
   automatic token bootstrap succeeds and the config becomes 0600.

Gate: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test --all-targets`; cross-build `./build-mips.sh` and
inspect the ELF/size.

## 10. Dependencies

- `serde_json` for RCI and the control protocol.
- `toml_edit` for comment-preserving config writes. It is already in the
  dependency graph through `toml 0.8.23` (`toml_edit 0.22.27`), so adding it
  directly adds no new transitive tree; the direct dependency is pinned to the
  same 0.22 line.
- Everything else stays as-is. No async runtime, no shelling out for normal
  RCI/PPPoE control.
- If `toml_edit` causes build/size problems on MIPS, fall back to a
  sentinel-based editor inside `configedit.rs`; the public editing API stays
  the same.

## 11. Rollout

- Version 0.2.0. No config migration required; new fields default.
- Installer/package layout unchanged. The init script still runs `wolguard
  --config ... run`; its stop timeout becomes 15 s.
- README rewritten around the new setup flow and management console; default
  config comments updated; troubleshooting covers RCI tokens, SSH drop,
  ISP/rate limits, reload, recovery, and grey classification.
- Keep `check-config` as an alias so existing scripts and docs do not break.
- Sequence:
  1. Core: RCI (read/write/probe/token), classification, PPPoE state machine,
     state file/recovery, config, WAN thread, logging, tests; CLI `pppoe
     list/restart/watch`, status integration.
  2. Management: control socket, TUI client/tabs/forms, config editor, host
     and settings commands, setup wizard, doctor, styling.
  3. Docs/polish: README, default config, help, version, packaging check,
     manual device validation.

## 12. Decisions made (for review)

- Trigger defaults to `on-change`; `always` is an option. `on-change`
  evaluates at daemon start/target enable and hunts if the address is grey,
  then hunts on later session changes; it does not re-hunt a stable grey
  address on every poll.
- Recovery touches only interfaces this daemon took down (`pending_down`);
  interfaces that were already down are left alone.
- Duplication protection uses `flock` + PID + socket; clients attach to the
  running daemon.
- `wol_enabled` and `[pppoe] enabled` plus per-host/per-target `enabled`
  switches let the user turn off unused parts.
- Multiple PPPoE targets are supported; the picker acts on the selected one.
- The TUI becomes a daemon client via a Unix control socket; `--standalone`
  remains for development.
- PPPoE recovery is guaranteed with a persistent state file, startup
  reconciliation, and an unconditional up path that bypasses rate limits.
  Shutdown interrupts waits and restores immediately; the init script timeout
  is 15 s.
- RCI write forms are probed in several documented shapes (string and boolean
  `up` flags on three endpoint layouts); every write is verified by readback;
  the working form is cached; a no-op is a hard error, not a silent attempt.
- Session changes are detected by address change, uptime reset, observed
  down→up, and `last-change` when available; `doctor` reports which signals
  the firmware exposes.
- `pppoe restart` is one down/up cycle, daemon-only, rate-limited; `--force`
  bypasses the limit. Direct one-shot restart is refused to avoid leaving the
  WAN down.
- `watch` creates/enables a target without overwriting rules; `unwatch`
  disables and keeps rules; `remove` deletes the block with confirmation.
- A config with zero hosts is valid when at least one PPPoE target exists;
  both empty is an error.
- RCI token bootstrap is automatic via `ndmc` on 401/403 (config
  `auto_token = true`), stored through `configedit` with mode 0600; a manual
  `rci.token` and `wolguard rci token` remain available.
- Config is written with `toml_edit`; comments are preserved; a revision check
  prevents silent overwrites between concurrent editors.
- The control protocol is newline-delimited JSON with a simple one-request
  connection model.
