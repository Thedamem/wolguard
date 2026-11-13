# wolguard

Wake-on-LAN guard with an interactive terminal dashboard for **Keenetic Giga KN-1011** routers running **Entware**.

`wolguard` watches a list of LAN hosts from the router: it probes them with ICMP and, when a host stops answering, sends a Wake-on-LAN magic packet. It is built for setups where the server's motherboard cannot power on by itself after the mains power returns — the router stays up, notices that the server is offline and wakes it. A compact ratatui dashboard shows live status over SSH.

[![CI](https://github.com/Thedamem/wolguard/actions/workflows/ci.yml/badge.svg)](https://github.com/Thedamem/wolguard/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Thedamem/wolguard?display_name=tag&sort=semver)](https://github.com/Thedamem/wolguard/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Features

- **ICMP watchdog** — raw-socket echo probes from the router, no external `ping` binary.
- **Correct Wake-on-LAN routing** — the magic packet goes to the LAN broadcast address derived from the kernel routing table and is bound to the LAN interface, so it never leaks to the WAN (a plain `255.255.255.255` on a router usually follows the default route).
- **Sane wake policy** — per-host intervals, failure thresholds, retry cooldown and an hourly wake limit, so a host that cannot be woken is not flooded with packets.
- **`wake_on_start`** — wake the server right after a router reboot even if the daemon has never seen it online.
- **Ratatui dashboard** — host table with states, RTT and wake counters, plus a live log panel; adapts to narrow SSH terminals.
- **Entware package** — a proper `mipsel-3.4` `.ipk` with an init script (`S99wolguard`).
- **One static binary** — MIPS32r2, soft-float, linked statically against musl; no runtime dependencies on the router.

## Requirements

- Keenetic Giga **KN-1011** (MT7621, MIPS32 little-endian, soft-float) with [Entware](https://entware.net/) installed and `/opt` mounted. Other Keenetic models need a rebuild for their architecture.
- Root shell for installation — Entware init scripts already run as root, and the daemon needs raw ICMP and `SO_BINDTODEVICE`.
- On the server: Wake-on-LAN enabled in UEFI/BIOS and on the network card, and ICMP echo allowed by its firewall (see [Troubleshooting](#troubleshooting)).
- A wired link between the router and the server is recommended. The server should have a stable IP address (a static lease or a reservation on the router).

## Installation

### One command from GitHub

```sh
wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
```

or with `curl`:

```sh
curl -fsSL https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
```

`get.sh` downloads the latest release bundle, verifies its SHA-256 checksum, unpacks it and runs the installer. Options are passed through to the installer, for example:

```sh
wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh -s -- --no-start
```

### Entware package

```sh
wget -O /tmp/wolguard.ipk \
  https://github.com/Thedamem/wolguard/releases/latest/download/wolguard-mipsel.ipk
opkg install /tmp/wolguard.ipk
```

You can also copy the `.ipk` into `/opt/install/` on the Entware USB drive — the Keenetic OPKG component unpacks packages placed there automatically.

### Manual install from a release bundle

```sh
wget -O /tmp/wolguard-install.tar.gz \
  https://github.com/Thedamem/wolguard/releases/latest/download/wolguard-install.tar.gz
mkdir -p /tmp/wolguard-install && tar -xzf /tmp/wolguard-install.tar.gz -C /tmp/wolguard-install
sh /tmp/wolguard-install/install.sh
```

The installer copies `/opt/bin/wolguard`, creates `/opt/etc/wolguard.conf` (only if missing), installs `/opt/etc/init.d/S99wolguard`, validates the configuration and starts the service. Use `-h` to see the other options (`--no-start`, `--force`, `--uninstall`, `--purge`).

## Configure

Edit `/opt/etc/wolguard.conf`:

```toml
# Probe every host this often and consider it dead after this many failures.
check_interval_secs = 10
ping_timeout_ms = 1000
failures_before_wake = 3

# Wake policy: at least 10 minutes between wakes, at most 6 wakes per hour.
wake_retry_secs = 600
max_wakes_per_hour = 6
wake_on_start = true

# "auto" derives the directed LAN broadcast address from the routing table,
# or set it explicitly, e.g. "192.168.1.255".
broadcast = "auto"
wol_port = 9
log_file = "/opt/var/log/wolguard.log"

[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
```

Then validate and restart the service:

```sh
/opt/bin/wolguard --config /opt/etc/wolguard.conf check-config
/opt/etc/init.d/S99wolguard restart
```

### Global options

| Key | Default | Description |
| --- | --- | --- |
| `check_interval_secs` | `10` | How often every enabled host is probed |
| `ping_timeout_ms` | `1000` | ICMP echo timeout |
| `failures_before_wake` | `3` | Consecutive failed probes before a magic packet is sent |
| `wake_retry_secs` | `600` | Minimum delay between two wake attempts for one host |
| `max_wakes_per_hour` | `6` | Hourly wake limit per host |
| `wake_on_start` | `true` | Wake hosts after a daemon start even if they were never seen online |
| `broadcast` | `"auto"` | `"auto"` or an explicit IPv4 broadcast address |
| `wol_port` | `9` | Wake-on-LAN UDP port |
| `log_file` | `/opt/var/log/wolguard.log` | Log file, or `"-"` for stdout |

### Host options

| Key | Default | Description |
| --- | --- | --- |
| `name` | required | Identifier used by `wolguard wake <name>` and the dashboard |
| `ip` | required | Static IPv4 address of the host on the LAN |
| `mac` | required | `AA:BB:CC:DD:EE:FF`, `aa-bb-cc-dd-ee-ff` or `aabb.ccdd.eeff` |
| `enabled` | `true` | Set to `false` to keep a host in the config but stop monitoring it |
| `check_interval_secs`, `wake_retry_secs`, `failures_before_wake`, `max_wakes_per_hour` | inherited | Per-host overrides of the global values |

## Usage

```sh
wolguard [--config PATH] <command>

  run            run the monitor loop in the foreground (used by the init script)
  tui            interactive dashboard (requires a TTY)
  wake <host>    send one magic packet now; <host> is a name, an IP or a 1-based index
  check-config   validate the configuration and print resolved settings
```

### Dashboard

```sh
/opt/bin/wolguard tui
```

```text
┌ wolguard 0.1.0 ────────────────────────────────────────────────────────────────┐
│ config /opt/etc/wolguard.conf   uptime 2h 14m   wake 192.168.1.255 (br0) udp 9 │
├ hosts (2) ─────────────────────────────────────────────────────────────────────┤
│ #   Name     IP             MAC                State    RTT      Last seen Wakes │
│ > 1  server    192.168.1.10   aa:bb:cc:dd:ee:ff  online   0.8 ms   4s         3 │
│   2  nas       192.168.1.11   aa:bb:cc:dd:ee:01  offline  -        6m        12 │
├ log ───────────────────────────────────────────────────────────────────────────┤
│ 20:14:02 [INFO] server (192.168.1.10) is online, rtt 0.8 ms                    │
│ 20:14:12 [WARN] nas (192.168.1.11) is offline (3 failed checks)                │
│ 20:14:12 [INFO] Wake-on-LAN sent to nas (aa:bb:cc:dd:ee:01) via 192.168.1.255   │
├ keys ──────────────────────────────────────────────────────────────────────────┤
│ up/down select   w wake   e enable/disable   q quit                            │
└────────────────────────────────────────────────────────────────────────────────┘
```

Keys: `q` / `Esc` / `Ctrl-C` quit, `Up` / `Down` (or `k` / `j`) select a host, `w` sends a magic packet to the selected host, `e` toggles monitoring for it. On terminals narrower than 95 columns secondary columns are hidden so the table stays readable.

### Logs

```sh
tail -f /opt/var/log/wolguard.log
/opt/etc/init.d/S99wolguard status
```

## How it works

1. Every enabled host is probed with an ICMP echo request over a raw socket. A host is considered offline after `failures_before_wake` consecutive failures.
2. For an offline host the daemon resolves a wake target: unless `broadcast` is set explicitly, `/proc/net/route` is searched for the route that serves the host IP, the interface netmask is read with `SIOCGIFNETMASK`, and the directed broadcast address (`ip | ~mask`) is computed. The magic packet is bound to that interface with `SO_BINDTODEVICE`.
3. A 102-byte magic packet (6 × `0xFF` followed by the MAC address 16 times) is sent over UDP to the wake target.
4. While the host stays offline, attempts repeat no more often than `wake_retry_secs` and never more than `max_wakes_per_hour` times per hour.
5. When the host answers again, it is logged as online and the failure counter is reset.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| The server stays offline even though it is up | Its firewall blocks ICMP echo. Allow ICMP on the server (on Windows: File and Printer Sharing / `ICMPv4 echo` rule, or `netsh advfirewall firewall add rule name="ICMPv4" protocol=icmpv4:8,any dir=in action=allow`). Otherwise the daemon treats it as switched off. |
| No wake packet is sent | Run `wolguard check-config` and check the reported wake target. If the LAN route cannot be derived, set `broadcast = "192.168.1.255"` explicitly. |
| Wake packet is sent but the server does not start | Enable Wake-on-LAN in UEFI/BIOS (`Power On by PCI-E/PCI`, disable ErP/EuP) and on the NIC (`Wake on Magic Packet`); disable Windows Fast Startup; check that the MAC in the config is the wired NIC MAC. |
| `cannot bind ... br0: Operation not permitted` | Run as root (the Entware init script does). |
| `cannot open a raw ICMP socket` | Same: raw sockets require root. |
| Wakes every 10 minutes with no effect | This is the retry policy for a host that never comes back. Lower `max_wakes_per_hour` or set `enabled = false` for that host. |

## Building from source

The shipped binary is built for `mipsel-unknown-linux-musl` (a tier-3 target, so the standard library is rebuilt from source with nightly `-Z build-std` and linked with Zig).

```sh
# Toolchain used by the release build:
rustup toolchain install nightly --profile minimal --component rust-src
cargo install --locked cargo-zigbuild
# Zig 0.13+ is required; build-mips.sh prints the exact download commands if it is missing.

./build-mips.sh                      # -> dist/wolguard-mips
sh packaging/make-ipk.sh dist/wolguard-mips dist   # -> dist/wolguard_<ver>_mipsel-3.4.ipk
sh packaging/make-dist.sh            # -> dist/wolguard-install.tar.gz + distributed checksums
```

Host-native development and tests do not need the cross toolchain:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- --config ./wolguard.conf check-config
cargo run -- --config ./wolguard.conf tui
```

## Project layout

```text
src/
  main.rs      CLI, modes, signal handling
  config.rs    TOML configuration, validation
  ping.rs      raw ICMP echo probing
  net.rs       routing table parsing, LAN broadcast resolution
  wol.rs       magic packet construction and delivery
  monitor.rs   probing/wake state machine
  tui.rs       ratatui dashboard
  log.rs       file/stdout/ring-buffer logging
  model.rs     shared types between monitor and dashboard
  util.rs      formatting helpers
packaging/
  make-ipk.sh    Entware .ipk builder
  make-dist.sh   install bundle + checksums for releases
  S99wolguard    Entware init script
  wolguard.conf  default configuration
build-mips.sh    reproducible MIPS cross-build
install.sh       on-router installer
get.sh           one-command GitHub installer
.github/workflows
  ci.yml         fmt, clippy, tests, shell syntax
  release.yml    tag-driven MIPS build and GitHub release
```

## Caveats

- The binary targets the KN-1011's MIPS32 soft-float CPU. Other Keenetic models need a rebuild for their own architecture.
- Wake-on-LAN must be enabled on the server side; no userspace daemon can wake a machine whose NIC ignores magic packets.
- Hardware validation on a physical KN-1011 is done through the protocol-level integration tests in the repository; reports from real devices are welcome in the issue tracker.

## License

MIT — see [LICENSE](LICENSE).
