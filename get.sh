#!/bin/sh
#
# get.sh - one-command installer for wolguard.
#
# Downloads the latest GitHub release bundle, verifies its SHA-256 checksum,
# unpacks it and runs the bundled install.sh with the arguments you pass.
#
# Usage:
#   wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
#   wget -qO- https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh -s -- --no-start
#   curl -fsSL https://raw.githubusercontent.com/Thedamem/wolguard/main/get.sh | sh
#
# Environment:
#   WOLGUARD_REPO=<owner/repo>     alternate repository (default: Thedamem/wolguard)
#   WOLGUARD_VERSION=<tag>         release tag to install (default: latest)
#   WOLGUARD_BASE_URL=<url>        override the release download base URL
#   WOLGUARD_SKIP_SHA256=1         skip checksum verification (not recommended)
#   WOLGUARD_VERIFY=1              download and verify only, do not install
#
# The checksum file is produced by the release workflow; when no SHA-256 tool
# is available on the router the download continues with a warning.

set -eu

REPO="${WOLGUARD_REPO:-Thedamem/wolguard}"
VERSION="${WOLGUARD_VERSION:-latest}"
BASE_URL="${WOLGUARD_BASE_URL:-https://github.com/${REPO}/releases/${VERSION}/download}"
BUNDLE="wolguard-install.tar.gz"
CHECKSUMS="SHA256SUMS"

VERIFY_ONLY="${WOLGUARD_VERIFY:-0}"

log() { printf 'get: %s\n' "$*"; }
warn() { printf 'get: WARNING: %s\n' "$*" >&2; }
die() { printf 'get: ERROR: %s\n' "$*" >&2; exit 1; }

# download URL DEST
download() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "neither curl nor wget is available; install one first: opkg update && opkg install curl"
	fi
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	elif command -v busybox >/dev/null 2>&1 && busybox sha256sum "$1" >/dev/null 2>&1; then
		busybox sha256sum "$1" | awk '{print $1}'
	elif command -v sha256 >/dev/null 2>&1; then
		sha256 -q "$1"
	else
		return 1
	fi
}

WORK="${TMPDIR:-/tmp}/wolguard-get.$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM

mkdir -p "$WORK" || die "cannot create $WORK"

log "downloading $BASE_URL/$BUNDLE"
download "$BASE_URL/$BUNDLE" "$WORK/$BUNDLE" ||
	die "download failed; check the network and TLS support (on Entware: opkg install curl ca-bundle)"

if [ "${WOLGUARD_SKIP_SHA256:-0}" != "1" ]; then
	if download "$BASE_URL/$CHECKSUMS" "$WORK/$CHECKSUMS" 2>/dev/null; then
		EXPECTED="$(awk -v name="$BUNDLE" '$2 == name || $2 == "./" name { print $1; exit }' "$WORK/$CHECKSUMS")"
		[ -n "$EXPECTED" ] || die "no checksum entry for $BUNDLE in $CHECKSUMS"
		if ACTUAL="$(sha256_of "$WORK/$BUNDLE")"; then
			if [ "$ACTUAL" != "$EXPECTED" ]; then
				die "checksum mismatch for $BUNDLE (expected $EXPECTED, got $ACTUAL)"
			fi
			log "checksum verified"
		else
			warn "no SHA-256 tool found; checksum NOT verified"
		fi
	else
		warn "release has no $CHECKSUMS; checksum NOT verified"
	fi
fi

tar -xzf "$WORK/$BUNDLE" -C "$WORK" || die "cannot extract $BUNDLE"
[ -f "$WORK/install.sh" ] || die "bundle does not contain install.sh"

if [ "$VERIFY_ONLY" = "1" ]; then
	log "bundle downloaded and unpacked OK (verify-only mode)"
	exit 0
fi

log "running the installer"
sh "$WORK/install.sh" "$@"
