# wolguard

Wake-on-LAN guard and **PPPoE white-IP watchdog** with a management console for
**Keenetic Giga KN-1011** routers running **Entware**.

`wolguard` watches LAN hosts from the router, wakes them with a magic packet when
they stop answering, and — independently — watches PPPoE interfaces. When a
PPPoE session comes back with a **grey** IPv4 address (private or CGNAT), it can
restart the session until a **white** (globally routable) address appears, within
configurable rules. A ratatui dashboard and a CLI are clients of the running
daemon, so hosts, PPPoE targets, rules and global settings can be changed
without hand-editing TOML.

[![CI](https://github.com/Thedamem/wolguard/actions/workflows/ci.yml/badge.svg)](https://github.com/Thedamem/wolguard/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Thedamem/wolguard?display_name=tag&sort=semver)](https://github.com/Thedamem/wolguard/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Features

- **ICMP watchdog** — raw-socket echo probes from the router, no external
  `ping` binary.
- **Correct Wake-on-LAN routing** — the magic packet goes to the LAN broadcast
  address derived from the kernel routing table and is bound to the LAN
  interface, so it never leaks to the WAN.
- **Sane wake policy** — per-host intervals, failure thresholds, retry cooldown
  and an hourly wake limit.
- **PPPoE white-IP watchdog** — scans PPPoE interfaces through the router's
  local RCI, classifies IPv4 addresses as white or grey, and restarts a session
  when it reconnects with a grey address. The hunt continues until a white
  address appears or the hourly limit is reached.
- **Trigger modes** — `on-change` (default) evaluates at start/enable and on
  session changes; `always` also fixes a stable grey address.
- **Crash-safe recovery** — a persistent marker records an interface the daemon
  took down. If the daemon is stopped or killed mid-cycle, the next start
  brings **that** interface back up; interfaces that were already down for other
  reasons are never touched.
- **Management console** — the TUI and CLI talk to the running daemon through a
  local Unix socket. A second `run` never starts a duplicate monitor; it reports
  the running daemon instead.
- **Editable configuration** — hosts, PPPoE targets, rules and global settings
  can be changed from the TUI or CLI. Comments are preserved, writes are atomic,
  and a `.bak` copy is kept.
- **RCI token bootstrap** — on KeeneticOS versions that require an access token,
  `wolguard` can generate one through `ndmc` and store it automatically.
- **Entware package** — a `mipsel-3.4` `.ipk` with an init script
  (`S99wolguard`).
- **One static binary** — MIPS32r2, soft-float, linked statically against musl;
  no runtime dependencies on the router.

## Requirements

- Keenetic Giga **KN-1011** (MT7621, MIPS32 little-endian, soft-float) with
  [Entware](https://entware.net/) installed and `/opt` mounted. Other Keenetic
  models need a rebuild for their architecture.
- Root shell for installation. Entware init scripts already run as root, and the
  daemon needs raw ICMP, `SO_BINDTODEVICE` and RCI access.
- Local RCI on `http://127.0.0.1:79`. It is available on KeeneticOS by default;
  KeeneticOS 5.2+ may require an access token (`X-NDMA-TKN`). wolguard can
  generate one with `ndmc` when `rci.auto_token = true` (the default).
- For Wake-on-LAN: WoL enabled in UEFI/BIOS and on the NIC, and ICMP echo
  allowed by the server firewall.
- A wired link between the router and the server is recommended, with a stable
  IP address (a static lease or a reservation).

## Installation

### One command from GitHub

```sh
wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
```

or with `curl`:

```sh
curl -fsSL https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
```

`get.sh` downloads the latest release bundle, verifies its SHA-256 checksum,
unpacks it and runs the installer. Options are passed through:

```sh
wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh -s -- --no-start
```

### Entware package

```sh
wget -O /tmp/wolguard.ipk \
  https://github.com/Thedamem/wolguard/releases/latest/download/wolguard-mipsel.ipk
opkg install /tmp/wolguard.ipk
```

You can also copy the `.ipk` into `/opt/install/` on the Entware USB drive — the
Keenetic OPKG component unpacks packages placed there automatically.

### Manual install from a release bundle

```sh
wget -O /tmp/wolguard-install.tar.gz \
  https://github.com/Thedamem/wolguard/releases/latest/download/wolguard-install.tar.gz
mkdir -p /tmp/wolguard-install && tar -xzf /tmp/wolguard-install.tar.gz -C /tmp/wolguard-install
sh /tmp/wolguard-install/install.sh
```

The installer copies `/opt/bin/wolguard`, creates `/opt/etc/wolguard.conf` (only
if missing), installs `/opt/etc/init.d/S99wolguard`, validates the configuration
and starts the service. Use `-h` for `--no-start`, `--force`, `--uninstall` and
`--purge`.

## Quick start

```sh
# Interactive first-run setup: hosts, PPPoE target, rules.
/opt/bin/wolguard setup

# The dashboard (a TUI client of the running daemon) on a TTY.
/opt/bin/wolguard

# One-shot status that also works in the Keenetic web console.
/opt/bin/wolguard status

# Scan the router's PPPoE interfaces.
/opt/bin/wolguard pppoe

# Diagnose RCI, ICMP, the service and the configuration.
/opt/bin/wolguard doctor
```

The default configuration path is `/opt/etc/wolguard.conf`. Global options:

```text
wolguard [--config PATH] [--ascii] [--json] <command>
```

`--ascii` forces plain ASCII output. `--json` is supported by `status`, `hosts
list`, `hosts leases`, `pppoe list`, `settings show` and `config validate`.

## Usage

Run `wolguard` with **no arguments** on a TTY for the dashboard. Without a TTY
(the Keenetic web console), the same command prints a one-shot status.

### Command reference

```text
run                         monitor loop in the foreground (init script)
tui [--standalone]          dashboard client; --standalone runs a local monitor
status                      one-shot snapshot (daemon snapshot or live probe)
setup                       interactive first-run wizard

hosts [list]                list configured hosts
hosts add                   add a host (interactive on a TTY)
hosts edit <name>           edit a host
hosts remove <name>         remove a host
hosts enable <name>         enable monitoring
hosts disable <name>        disable monitoring
hosts wake <name>           send one magic packet
hosts leases                DHCP bindings reported by RCI
wake <host>                 alias of `hosts wake`

pppoe [list|scan]           scan and list PPPoE interfaces
pppoe show <id>             detailed interface view
pppoe restart <id>          one down/up cycle (requires the daemon)
pppoe test <id>             one cycle with a live result (--dry-run to classify only)
pppoe watch <id>            add/enable a watched target (rules are not overwritten)
pppoe unwatch <id>          disable a target, keeping its rules
pppoe remove <id>           delete a target and its rules (confirmation)
pppoe rules <id> [--edit]   show or edit target rules

settings [show]             resolved settings
settings set <key> <value>  change one setting
settings edit               interactive subset of settings
config show|validate|path   inspect the TOML file
logs [-n N] [--follow]      daemon log ring buffer or log file
service start|stop|restart|status   Entware init script
doctor                      environment/config/RCI diagnostics
rci show|token               RCI URL/token helpers
check-config                alias of `config validate`
```

Interactive commands require a TTY; scripts should use the explicit flags, for
example:

```sh
wolguard hosts add --name server --ip 192.168.1.10 --mac AA:BB:CC:DD:EE:FF
wolguard pppoe watch PPPoE0
wolguard settings set pppoe.trigger always
wolguard pppoe restart PPPoE0 --force
```

`hosts add` on a TTY offers the DHCP leases reported by RCI, so the IP and MAC
can be selected instead of typed. `wolguard hosts leases` lists them on its own.

### Dashboard

```sh
/opt/bin/wolguard tui
```

The dashboard attaches to the running daemon through
`/tmp/wolguard.sock`. It also works when the daemon is stopped: it shows
a banner, offers `S` to start the service and `R` to retry.

```text
┌ wolguard 0.2.0  daemon running (pid 8647)  WOL on  config /opt/etc/wolguard.conf
│ PPPoE PPPoE0 100.70.12.34 grey hunting
│ 1 Overview   2 Hosts   3 WAN / PPPoE   4 Settings   5 Logs
├ hosts (2) ─────────────────────────────────────────────────────────────────────
│ #   Name     IP             MAC                State    RTT      Last seen Wakes
│ > 1 server   192.168.1.10   aa:bb:cc:dd:ee:ff  online   0.8 ms   4s         3
│   2 nas      192.168.1.11   aa:bb:cc:dd:ee:01  offline  -        6m        12
├ PPPoE (1) ─────────────────────────────────────────────────────────────────────
│ #   ID       Provider      IPv4            Class  State   Watch  Phase     Restarts
│ > 1 PPPoE0   Дом. интернет 100.70.12.34    grey   up/yes  on     waiting-ip       2
└ keys: j/k select  Tab tabs  r restart  t test  w watch  e rules  q quit
```

Tabs and keys:

| Tab | Keys |
| --- | --- |
| Overview | `w` wake the selected host, `d` remove, `e` edit |
| Hosts | `w` wake, `Space` enable/disable, `a` add, `e` edit, `d` remove |
| WAN / PPPoE | `r` restart, `t` test, `w` watch/unwatch, `e` rules, `d` remove, `s` rescan |
| Settings | `e` edit the settings form |
| Logs | `f` follow, `j`/`k` scroll, `g`/`G` top/bottom, `i`/`W`/`E`/`A` level filter |

Global keys: `1`–`5` or `Tab` switch tabs, `q`/`Esc` quit, `R` reconnect,
`S` start the service. The dashboard restores the terminal on exit and on panic.

`tui --standalone` runs an in-process monitor without a daemon. It refuses to
start while a daemon is alive; it is intended for development.

### Status without a dashboard

`wolguard status` works without a TTY. If the daemon is running it prints the
live snapshot; otherwise it probes hosts directly and, when PPPoE targets are
configured, scans RCI.

```text
wolguard 0.2.0 status
config: /opt/etc/wolguard.conf
daemon: running (pid 8647), control /tmp/wolguard.sock
uptime: 2h 14m
WOL: enabled
wake policy: after 5 failed checks, retry every 600 s, at most 6/hour
wake target: 192.168.2.255 (br0)
session signals: address, uptime, down-up

PPPoE interfaces (1)
#   ID       Provider      IPv4            Class  State   Watch  Phase  Restarts
1   PPPoE0   Дом. интернет 100.70.12.34    grey   up/yes  on     idle          2
```

### Logs and service

```sh
/opt/bin/wolguard logs -n 100
/opt/bin/wolguard logs --follow
/opt/etc/init.d/S99wolguard status
tail -f /opt/var/log/wolguard.log
```

## Configuration

The file is TOML. Unknown keys are rejected so that typos do not silently fall
back to defaults. It can still be edited by hand, but the TUI and CLI write it
through a comment-preserving editor:

- comments, key order and unrelated settings survive;
- the resulting file is parsed and validated before the atomic replace;
- the previous file is kept as `/opt/etc/wolguard.conf.bak`;
- a concurrent external edit is detected and the save is refused (the UI shows
  the conflict).

### Global options

| Key | Default | Description |
| --- | --- | --- |
| `wol_enabled` | `true` | Global LAN host monitoring switch; PPPoE is independent |
| `check_interval_secs` | `10` | How often every enabled host is probed |
| `ping_timeout_ms` | `1000` | ICMP echo timeout |
| `failures_before_wake` | `3` | Consecutive failed probes before a magic packet |
| `wake_retry_secs` | `600` | Minimum delay between two wake attempts |
| `max_wakes_per_hour` | `6` | Hourly wake limit per host |
| `wake_on_start` | `true` | Wake hosts after a daemon start even if never seen online |
| `broadcast` | `"auto"` | `"auto"` or an explicit IPv4 broadcast address |
| `wol_port` | `9` | Wake-on-LAN UDP port |
| `log_file` | `/opt/var/log/wolguard.log` | Log file, or `"-"` for stdout |
| `pid_file` | `/opt/var/run/wolguard.pid` | PID/lock file |
| `control_socket` | `/tmp/wolguard.sock` | Local control socket (tmpfs, so it works on vfat Entware volumes) |
| `init_script` | `/opt/etc/init.d/S99wolguard` | Used by `wolguard service` |
| `state_file` | `/opt/var/run/wolguard.state` | PPPoE recovery/observation state |
| `verbose_probes` | `false` | Log every failed probe instead of only the first |

### RCI

```toml
[rci]
url = "http://127.0.0.1:79"
token = ""              # optional X-NDMA-TKN (KeeneticOS 5.2+)
timeout_ms = 2000
auto_token = true       # generate a token via ndmc on 401/403
ndmc_path = "ndmc"      # optional override; common paths are tried
```

When `auto_token` is enabled and RCI returns 401/403, the daemon runs
`ndmc -c "authentication token generate wolguard"`, stores the value in the
configuration (mode 0600) and retries. `wolguard rci token` forces the same
bootstrap manually. On routers where `ndmc` is unavailable, set `rci.token` by
hand or create a token in the Keenetic web interface.

### PPPoE defaults

```toml
[pppoe]
enabled = true
trigger = "on-change"       # "on-change" | "always"
poll_secs = 15
down_secs = 5
settle_secs = 60
retry_secs = 45
max_restarts_per_hour = 12
white_cidrs = []
grey_cidrs = []
```

`white_cidrs` and `grey_cidrs` are extra classification patterns. An explicit
grey pattern wins over everything; an explicit white pattern can override the
built-in special-use table. Defaults:

- built-in grey: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`,
  `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.0.2.0/24`,
  `192.88.99.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `198.51.100.0/24`,
  `203.0.113.0/24`, `224.0.0.0/4`, `240.0.0.0/4`, `255.255.255.255/32`;
- everything else is white (public).

### PPPoE targets

```toml
[[pppoe.target]]
interface = "PPPoE0"
# Optional per-target overrides:
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

A target block is what "watch" means. `wolguard pppoe watch <id>` creates or
enables it without touching existing rules; `unwatch` sets `enabled = false`
and keeps the rules; `remove` deletes the block. A configuration with zero
`[[host]]` entries is valid when at least one `[[pppoe.target]]` exists.

### Hosts

```toml
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
enabled = true
# Optional overrides:
# check_interval_secs = 10
# wake_retry_secs = 600
# failures_before_wake = 3
# max_wakes_per_hour = 6
```

`mac` accepts `AA:BB:CC:DD:EE:FF`, `aa-bb-cc-dd-ee-ff` and `aabb.ccdd.eeff`.

## PPPoE white-IP watchdog

1. Every scan reads `GET /rci/show/interface/` and keeps `type = "PPPoE"`.
2. An address is classified as white/grey by the explicit patterns and the
   built-in special-use table.
3. `trigger = "on-change"` (default) evaluates immediately when the daemon
   starts or a target is enabled, and afterwards on a session change:
   address change, `uptime` reset, an observed down→up transition, or a changed
   `last-change` value when the firmware exposes it. It does not re-hunt a
   stable grey address on every poll.
   `trigger = "always"` also hunts a stable grey address.
4. A hunt writes the recovery marker, sends `up=false`, waits `down_secs`, sends
   `up=true`, and waits up to `settle_secs` for a usable address. A grey result
   schedules another cycle after `retry_secs`; a white result stops the hunt.
5. At most `max_restarts_per_hour` cycles are allowed per target in a sliding
   hour. After the limit, the hunt waits for the oldest attempt to expire and
   then resumes if the address is still grey.
6. The RCI write shape is probed (string and boolean `up` flags on three
   endpoint layouts), every write is verified by reading the interface back,
   and the working form is cached. A `200 OK` with no state change is a failed
   actuator, not a successful attempt.

### Recovery guarantee

Only interfaces **taken down by this daemon** are restored. Before `up=false`
the daemon atomically writes `state_file` with the interface name. The marker is
cleared after a verified `up`. If the daemon is stopped, SIGKILLed or aborted
between the two calls, the next start restores that interface before monitoring
anything. An interface that was already down for another reason is never
brought up.

### Manual test

```sh
# Classify only: no down/up.
wolguard pppoe test PPPoE0 --dry-run

# One real cycle with a live result.
wolguard pppoe test PPPoE0

# One cycle without a result poll.
wolguard pppoe restart PPPoE0 --force
```

`pppoe test` is deliberately one-shot: if the cycle comes back grey it does not
start an automatic hunt; the next real session change re-arms the watchdog.
`pppoe restart` on a watched, enabled target continues with the normal automatic
hunt if the new address is still grey. Restarting the active WAN drops the SSH
session; the daemon survives and brings the interface back; reconnect after a
few seconds. `--force` bypasses the hourly limit, but never skips the recovery
marker or the readback verification.

## How it works

1. Every enabled host is probed with an ICMP echo over a raw socket. A host is
   offline after `failures_before_wake` consecutive failures.
2. For an offline host, the wake target is derived from `/proc/net/route`
   (longest-prefix match, interface netmask) unless `broadcast` is explicit. The
   magic packet is bound to that interface with `SO_BINDTODEVICE`.
3. A 102-byte magic packet (6 × `0xFF` + 16 × MAC) is sent over UDP.
4. The PPPoE watcher scans RCI, classifies addresses, and performs the restart
   cycle described above. WOL and PPPoE run in separate threads.
5. The daemon serves a Unix control socket. The TUI and CLI attach to it; only
   the daemon and the development-only `tui --standalone` mode run a monitor.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `RCI connection failed: Connection refused` | RCI is not listening at `127.0.0.1:79`. Check that this is a Keenetic router and `ndmc -c "show version"` works. Set `rci.url` if it listens elsewhere. |
| `RCI requires an access token` | Run `wolguard rci token`, or set `rci.token`. On 5.2+ the daemon can do this automatically with `rci.auto_token = true` when `ndmc` is available. |
| `pppoe restart` refuses to run | Restarts go through the daemon. Start it with `wolguard service start` (or the init script). A one-shot restart can drop SSH and leave the WAN down. |
| SSH disconnects during a restart | Expected when the PPPoE interface is the management path. The daemon is unaffected; reconnect. The marker guarantees `up` on the next start. |
| Address is classified wrongly | Add the range to `grey_cidrs` or `white_cidrs` (grey wins). `wolguard pppoe rules <id> --edit` edits a target. |
| Hunting forever | The hourly limit should bound it. Check `max_restarts_per_hour`, `retry_secs` and the log. `wolguard pppoe unwatch <id>` stops automatic hunting. |
| `interface not found in RCI` | The target id is stale or the interface was renamed. Run `wolguard pppoe` and watch/remove the current id. |
| The interface is left down | Run `wolguard doctor` and check `state_file`. A pending entry is restored at the next daemon start. Do not delete the state file manually while the daemon is running. |
| The server stays offline | Its firewall blocks ICMP echo. Allow ICMPv4 echo on the server. |
| Wake packet is sent but the server does not start | Enable WoL in UEFI/BIOS and on the NIC; disable Windows Fast Startup; verify the wired NIC MAC. |
| `raw ICMP` / `SO_BINDTODEVICE` permission errors | Run as root (the Entware init script does). |
| `wolguard tui` says the terminal is not interactive | Use an SSH terminal. The Keenetic web console has no PTY; use `wolguard status` there. |
| Two instances | `run` takes an exclusive `flock`, checks the PID file and binds the control socket. A second `run` prints the running PID and exits 0. Use `wolguard service status`. |
| Config save says it changed on disk | Another editor changed the file. Reload/edit again; the CLI refuses to overwrite silently. The previous file is in `.bak`. |
| TUI cannot connect | Start the service (`wolguard service start`). The TUI shows `S` to start it and `R` to retry. |
| `wolguard logs --follow` does nothing | The daemon may log to stdout (`log_file = "-"`). Use the service log or change `log_file`. |

## Building from source

The shipped binary is built for `mipsel-unknown-linux-musl` (a tier-3 target, so
the standard library is rebuilt from source with nightly `-Z build-std` and
linked with Zig).

```sh
# Toolchain used by the release build:
rustup toolchain install nightly --profile minimal --component rust-src
cargo install --locked cargo-zigbuild
# Zig 0.13+ is required; build-mips.sh prints the exact download commands if missing.

./build-mips.sh                      # -> dist/wolguard-mips
sh packaging/make-ipk.sh dist/wolguard-mips dist   # -> dist/wolguard_<ver>_mipsel-3.4.ipk
sh packaging/make-dist.sh            # -> dist/wolguard-install.tar.gz + checksums
```

Host-native development and tests do not need the cross toolchain:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -- --config ./wolguard.conf check-config
cargo run -- --config ./wolguard.conf tui --standalone
```

## Project layout

```text
src/
  main.rs       CLI, daemon lifecycle, status, hosts/PPPoE/settings commands
  config.rs     TOML configuration, validation, resolved settings
  configedit.rs comment-preserving atomic configuration edits
  control.rs    Unix control socket protocol (daemon + client)
  ipclass.rs    white/grey IPv4 classification and CIDR patterns
  rci.rs        minimal HTTP/JSON RCI client; write-form probing and token bootstrap
  pppoe.rs      pure PPPoE watchdog state machine
  wan.rs        PPPoE watcher thread, snapshot, recovery marker
  monitor.rs    host probing and wake policy
  ping.rs       raw ICMP echo probing
  net.rs        routing table parsing and LAN broadcast resolution
  wol.rs        magic packet construction and delivery
  tui.rs        ratatui dashboard, including the management-console client
  term.rs       router-proof terminal size detection
  prompt.rs     line-based interactive prompts
  style.rs      CLI colours, tables and ASCII fallback
  log.rs        file/stdout/ring-buffer logging with sequence numbers
  model.rs      shared snapshot/command types
  util.rs       formatting helpers
  statefile.rs  persistent PPPoE recovery/observation state
packaging/
  make-ipk.sh    Entware .ipk builder
  make-dist.sh   install bundle + checksums
  S99wolguard    Entware init script
  wolguard.conf  default configuration
build-mips.sh    reproducible MIPS cross-build
install.sh       on-router installer
get.sh           one-command GitHub installer
.github/workflows
  ci.yml         fmt, clippy, tests, shell syntax
  release.yml    tag-driven MIPS build and GitHub release
docs/
  superpowers/specs/2026-11-24-wolguard-pppoe-management-design.md
```

## Caveats

- The binary targets the KN-1011's MIPS32 soft-float CPU. Other Keenetic models
  need a rebuild for their architecture.
- RCI interface write shapes differ slightly across KeeneticOS versions. The
  client probes several documented forms (string and boolean `up` flags on three
  endpoint layouts), verifies each by readback and caches the working one.
  `wolguard doctor` and the log show what happened if none works.
- Wake-on-LAN must be enabled on the server side; no userspace daemon can wake a
  machine whose NIC ignores magic packets.
- Automatic RCI token bootstrap needs `ndmc`. Without it, set `rci.token`
  manually.
- The daemon runs as root, as do the TUI/CLI on the router.
- Hardware validation on a physical KN-1011 is done through the
  protocol-level tests in the repository; reports from real devices are welcome
  in the issue tracker.

## License

MIT — see [LICENSE](LICENSE).
