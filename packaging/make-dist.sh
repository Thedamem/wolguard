#!/bin/sh
#
# make-dist.sh - build the install bundle published with GitHub releases.
#
# The bundle is what get.sh downloads for the one-command installation:
# it contains install.sh, the packaged configuration/init script and the
# MIPS binary in the layout install.sh expects.
#
# Usage: sh packaging/make-dist.sh [BINARY] [OUTPUT_DIR]
#
#   BINARY      mipsel binary to bundle (default: dist/wolguard-mips)
#   OUTPUT_DIR  output directory (default: dist)
#
# Produces in OUTPUT_DIR:
#   wolguard-install.tar.gz   self-contained installer bundle (stable name)
#   wolguard-mipsel.ipk       stable-name copy of the versioned .ipk, if any
#   SHA256SUMS                checksums of all published release assets
#
# Requirements: POSIX sh, GNU tar, gzip, sha256sum, awk.

set -eu
export LC_ALL=C

die() {
	printf 'make-dist: ERROR: %s\n' "$*" >&2
	exit 1
}

info() {
	printf 'make-dist: %s\n' "$*"
}

ROOT=`CDPATH= cd "$(dirname "$0")/.." && pwd`
BIN_SRC=${1:-"$ROOT/dist/wolguard-mips"}
OUT_DIR=${2:-"$ROOT/dist"}
BUNDLE_NAME=wolguard-install.tar.gz

[ -f "$BIN_SRC" ] || die "binary not found: $BIN_SRC (run ./build-mips.sh first)"
[ -f "$ROOT/install.sh" ] || die "install.sh not found in $ROOT"
[ -f "$ROOT/packaging/wolguard.conf" ] || die "packaging/wolguard.conf not found"
[ -f "$ROOT/packaging/S99wolguard" ] || die "packaging/S99wolguard not found"

WORK=`mktemp -d "${TMPDIR:-/tmp}/wolguard-dist.XXXXXX"` || die "cannot create a temporary directory"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM

mkdir -p "$WORK/bundle/packaging" "$WORK/bundle/dist" "$OUT_DIR"

cp "$ROOT/install.sh" "$WORK/bundle/install.sh" || die "cannot copy install.sh"
cp "$ROOT/packaging/wolguard.conf" "$WORK/bundle/packaging/wolguard.conf" || die "cannot copy wolguard.conf"
cp "$ROOT/packaging/S99wolguard" "$WORK/bundle/packaging/S99wolguard" || die "cannot copy S99wolguard"
cp "$BIN_SRC" "$WORK/bundle/dist/wolguard-mips" || die "cannot copy $BIN_SRC"

chmod 755 "$WORK/bundle/install.sh" "$WORK/bundle/dist/wolguard-mips" "$WORK/bundle/packaging/S99wolguard"
chmod 644 "$WORK/bundle/packaging/wolguard.conf"

# Same conventions as the .ipk builder: GNU tar, sorted members, ownership 0:0,
# fixed mtime and gzip -n, so the bundle is byte-for-byte reproducible.
tar --format=gnu --numeric-owner --owner=0 --group=0 --sort=name --mtime=@0 \
	-C "$WORK/bundle" -cpf - . | gzip -n - >"$OUT_DIR/$BUNDLE_NAME" ||
	die "cannot create $OUT_DIR/$BUNDLE_NAME"

# Stable download name for `releases/latest/download/wolguard-mipsel.ipk`.
IPK=`ls "$OUT_DIR"/wolguard_*_mipsel-3.4.ipk 2>/dev/null | head -n 1 || true`
if [ -n "$IPK" ]; then
	cp -f "$IPK" "$OUT_DIR/wolguard-mipsel.ipk"
fi

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		die "sha256sum not found"
	fi
}

: >"$OUT_DIR/SHA256SUMS"
for asset in "$BUNDLE_NAME" wolguard-mips wolguard-mipsel.ipk; do
	[ -f "$OUT_DIR/$asset" ] || continue
	printf '%s  %s\n' "`sha256_of "$OUT_DIR/$asset"`" "$asset" >>"$OUT_DIR/SHA256SUMS"
done
for asset in "$OUT_DIR"/wolguard_*_mipsel-3.4.ipk; do
	[ -f "$asset" ] || continue
	name=`basename "$asset"`
	grep -q " $name\$" "$OUT_DIR/SHA256SUMS" 2>/dev/null && continue
	printf '%s  %s\n' "`sha256_of "$asset"`" "$name" >>"$OUT_DIR/SHA256SUMS"
done

info "bundle:     $OUT_DIR/$BUNDLE_NAME (`wc -c <"$OUT_DIR/$BUNDLE_NAME"` bytes)"
[ -f "$OUT_DIR/wolguard-mipsel.ipk" ] && info "ipk alias:  $OUT_DIR/wolguard-mipsel.ipk"
info "checksums:  $OUT_DIR/SHA256SUMS"
info "done"
