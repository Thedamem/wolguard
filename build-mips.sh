#!/usr/bin/env bash
# Cross-build wolguard for Keenetic Giga KN-1011 (Entware mipselsf-k3.4).
#
# Target: mipsel-unknown-linux-musl (MIPS32 LE, MIPS32r2, o32, soft-float ABI),
# statically linked, panic=abort. std is rebuilt from source with `-Z build-std`
# because this target is tier 3 and has no prebuilt standard library.
#
# The default artifact is a classic static ET_EXEC (relocation-model=static):
# the KeeneticOS kernel is 4.9 with an unknown patch set, and a non-PIE binary
# has no loader/self-relocation requirements at all.
#
# Usage (from the project root):   ./build-mips.sh
# Output:                          dist/wolguard-mips
#
# Overridable env:
#   WOLGUARD_CROSS_TARGET_DIR  cargo target dir (default: /tmp/wolguard-cross-target)
#   WOLGUARD_JOBS              parallelism (default: nproc)
#   WOLGUARD_PIE=1             build static-PIE (Rust default) instead of ET_EXEC

set -euo pipefail

# --- locate project root (script dir), works from anywhere -------------------
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"
PROJECT_ROOT="$SCRIPT_DIR"

TARGET="mipsel-unknown-linux-musl"
BIN_NAME="wolguard"
OUT_DIR="$PROJECT_ROOT/dist"
OUT_BIN="$OUT_DIR/wolguard-mips"
CARGO_TARGET_DIR="${WOLGUARD_CROSS_TARGET_DIR:-/tmp/wolguard-cross-target}"
JOBS="${WOLGUARD_JOBS:-$(nproc 2>/dev/null || echo 4)}"
export CARGO_TARGET_DIR

# --- PATH: cargo/rustup live in ~/.cargo/bin, zig in ~/.local/bin ------------
case ":$PATH:" in *":$HOME/.cargo/bin:"*) ;; *) PATH="$HOME/.cargo/bin:$PATH" ;; esac
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) PATH="$HOME/.local/bin:$PATH" ;; esac
export PATH

ok()   { printf '[ ok ] %s\n' "$*"; }
info() { printf '[ .. ] %s\n' "$*"; }
warn() { printf '[ !! ] %s\n' "$*" >&2; }
die()  { printf '[fail] %s\n' "$*" >&2; exit 1; }

section() { printf '\n== %s ==\n' "$*"; }

# --- dependency checks -------------------------------------------------------
section "Checking build dependencies"

# rustup
command -v rustup >/dev/null 2>&1 || die "rustup not found in PATH.
  Install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

# nightly toolchain (needed for -Z build-std)
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    die "nightly toolchain is not installed.
  Install it: rustup toolchain install nightly --profile minimal --component rust-src"
fi
NIGHTLY_RUSTC="$(rustup run nightly rustc --version 2>/dev/null || true)"
[ -n "$NIGHTLY_RUSTC" ] || die "nightly toolchain is registered but not usable.
  Reinstall it: rustup toolchain install nightly --profile minimal --component rust-src"
ok "$NIGHTLY_RUSTC"

# rust-src component (build-std reads std sources from it)
if ! rustup component list --toolchain nightly --installed 2>/dev/null | grep -qx 'rust-src'; then
    die "rust-src component is missing for nightly.
  Install it: rustup component add rust-src --toolchain nightly"
fi
ok "rust-src component present"

# cargo-zigbuild
if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    die "cargo-zigbuild not found in PATH.
  Install it: cargo install --locked cargo-zigbuild"
fi
ZIGBUILD_VER="$(cargo-zigbuild --version 2>/dev/null || true)"
[ -n "$ZIGBUILD_VER" ] || ZIGBUILD_VER="cargo-zigbuild (version unknown)"
ok "$ZIGBUILD_VER"

# zig (cargo-zigbuild calls it as the C compiler/linker)
find_zig() {
    if command -v zig >/dev/null 2>&1; then
        command -v zig
        return 0
    fi
    local c
    for c in "$HOME/.local/opt/zig/zig" "$HOME/.local/bin/zig"; do
        [ -x "$c" ] && { printf '%s\n' "$c"; return 0; }
    done
    for c in "$HOME"/.local/opt/zig-*/zig "$HOME"/.local/zig*/zig; do
        [ -x "$c" ] && { printf '%s\n' "$c"; return 0; }
    done
    return 1
}
ZIG_BIN="$(find_zig || true)"
if [ -z "$ZIG_BIN" ]; then
    die "zig not found.
  Install Zig (0.13-0.15 work with cargo-zigbuild 0.23.x):
    mkdir -p ~/.local/opt && cd /tmp
    curl -L -o zig.tar.xz https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz
    tar -xf zig.tar.xz -C ~/.local/opt
    ln -sfn ~/.local/opt/zig-x86_64-linux-0.15.2 ~/.local/opt/zig
    ln -sfn ~/.local/opt/zig/zig ~/.local/bin/zig
  (or: pip install ziglang --break-system-packages)"
fi
case ":$PATH:" in
    *":$(dirname "$ZIG_BIN"):"*) ;;
    *) PATH="$(dirname "$ZIG_BIN"):$PATH"; export PATH ;;
esac
# cargo-zigbuild 0.23+ honours this if zig is off PATH
export CARGO_ZIGBUILD_ZIG_PATH="$ZIG_BIN"
ZIG_VER="$("$ZIG_BIN" version 2>/dev/null || true)"
[ -n "$ZIG_VER" ] || die "zig at $ZIG_BIN is not executable: $ZIG_BIN version failed"
ok "zig $ZIG_VER ($ZIG_BIN)"

command -v file >/dev/null 2>&1 || warn "file not found (skipping binary report)"

# --- build -------------------------------------------------------------------
section "Building $BIN_NAME for $TARGET (release, static, soft-float)"
info "project:    $PROJECT_ROOT"
info "target dir: $CARGO_TARGET_DIR"
info "jobs:       $JOBS"
info "rustc:      $NIGHTLY_RUSTC"

# Cargo.lock is only rewritten when the dependency graph actually changed
# (cargo does not upgrade already-locked versions by itself). Report it rather
# than failing the build, so a stale lock cannot block it.
LOCK_BEFORE=""
[ -f Cargo.lock ] && LOCK_BEFORE="$(sha256sum Cargo.lock | cut -d' ' -f1)"

# Strip build-machine paths from the shipped binary: panic locations would
# otherwise embed ~/.cargo, the project directory and the build user name.
REMAP_FLAGS="--remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME/.rustup=/rust --remap-path-prefix=$PROJECT_ROOT=/wolguard"

# Default: classic static ET_EXEC. Rust's musl targets default to static-PIE,
# which relies on self-relocation at startup; on a 4.9 router kernel with an
# unknown patch set ET_EXEC is the safe choice, and PIE buys us nothing here.
# WOLGUARD_PIE=1 restores the static-PIE default.
if [ "${WOLGUARD_PIE:-0}" = "1" ]; then
    export RUSTFLAGS="${RUSTFLAGS:-} $REMAP_FLAGS"
    info "building static-PIE executable (WOLGUARD_PIE=1)"
else
    export RUSTFLAGS="${RUSTFLAGS:-} $REMAP_FLAGS -C relocation-model=static"
    info "building non-PIE static executable (default; WOLGUARD_PIE=1 for static-PIE)"
fi

# The tier-3 target's std is compiled from rust-src via build-std.
cargo +nightly zigbuild \
    --target "$TARGET" \
    --release \
    -Z build-std=std,panic_abort \
    -j "$JOBS"

if [ -n "$LOCK_BEFORE" ] && [ -f Cargo.lock ]; then
    LOCK_AFTER="$(sha256sum Cargo.lock | cut -d' ' -f1)"
    if [ "$LOCK_BEFORE" != "$LOCK_AFTER" ]; then
        warn "Cargo.lock was refreshed during the build (dependency graph changed)"
    fi
fi

BUILT="$CARGO_TARGET_DIR/$TARGET/release/$BIN_NAME"
[ -f "$BUILT" ] || die "build finished but $BUILT is missing"

# --- install to dist/ --------------------------------------------------------
section "Installing artifact"
mkdir -p "$OUT_DIR"
cp -f "$BUILT" "$OUT_BIN"
chmod 755 "$OUT_BIN"
ok "$OUT_BIN"

# --- verify ELF --------------------------------------------------------------
section "Verifying ELF"
FAILED=0

if command -v file >/dev/null 2>&1; then
    printf '%s\n' "$(file "$OUT_BIN")"
fi

if command -v readelf >/dev/null 2>&1; then
    HDR="$(readelf -h "$OUT_BIN")"
    ATTR="$(readelf -A "$OUT_BIN" 2>/dev/null || true)"

    printf '%s\n' "$HDR" | grep -q 'Class:.*ELF32' \
        && ok "ELF32" || { warn "not ELF32"; FAILED=1; }
    printf '%s\n' "$HDR" | grep -q 'little endian' \
        && ok "little endian" || { warn "not little endian"; FAILED=1; }
    printf '%s\n' "$HDR" | grep -q 'Machine:.*MIPS' \
        && ok "MIPS machine" || { warn "not a MIPS machine"; FAILED=1; }
    printf '%s\n' "$HDR" | grep -q 'mips32r2' \
        && ok "MIPS32r2 (rel2)" || { warn "not MIPS32r2"; FAILED=1; }
    printf '%s\n' "$HDR" | grep -q ' o32' \
        && ok "o32 ABI" || { warn "not o32 ABI"; FAILED=1; }
    printf '%s\n' "$ATTR" | grep -q 'FP ABI: Soft float' \
        && ok "soft-float ABI" || { warn "FP ABI is NOT soft-float"; FAILED=1; }
    if [ "${WOLGUARD_PIE:-0}" = "1" ]; then
        printf '%s\n' "$HDR" | grep -q 'Type:.*DYN' \
            && ok "static-PIE (ET_DYN, WOLGUARD_PIE=1)" || { warn "expected ET_DYN"; FAILED=1; }
    else
        printf '%s\n' "$HDR" | grep -q 'Type:.*EXEC' \
            && ok "ET_EXEC (non-PIE)" || { warn "expected ET_EXEC, got $(printf '%s\n' "$HDR" | grep 'Type:' | xargs)"; FAILED=1; }
    fi
    if readelf -l "$OUT_BIN" | grep -q 'INTERP'; then
        warn "binary has a PT_INTERP (dynamically linked)"; FAILED=1
    else
        ok "statically linked (no PT_INTERP)"
    fi

    printf '\n--- readelf -h ---\n'
    printf '%s\n' "$HDR" | grep -E 'Class|Data|Type|Machine|Flags'
    printf -- '--- readelf -A (header) ---\n'
    printf '%s\n' "$ATTR" | head -12
else
    warn "readelf not found, ELF attributes not verified"
fi

# Release hygiene: the binary must not leak build-machine paths.
if command -v strings >/dev/null 2>&1; then
    if strings "$OUT_BIN" | grep -q '/home/'; then
        warn "binary contains absolute /home paths (path remapping failed)"
        FAILED=1
    else
        ok "no absolute /home paths in the binary"
    fi
fi

SIZE="$(stat -c '%s' "$OUT_BIN" 2>/dev/null || wc -c < "$OUT_BIN")"
section "Result"
printf 'path:   %s\n' "$OUT_BIN"
printf 'size:   %s bytes (%s)\n' "$SIZE" "$(du -h "$OUT_BIN" | cut -f1)"
if command -v sha256sum >/dev/null 2>&1; then
    printf 'sha256: %s\n' "$(sha256sum "$OUT_BIN" | cut -d' ' -f1)"
fi

if [ "$FAILED" -ne 0 ]; then
    die "artifact built but ELF checks failed (see [ !! ] above)"
fi
ok "ready for Keenetic Giga KN-1011 (mipsel, soft-float, static)"
