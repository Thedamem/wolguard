#!/bin/sh
#
# make-ipk.sh - build an Entware opkg package (.ipk) for wolguard.
#
# Runs on the development machine, not on the router. The package contains:
#   /opt/bin/wolguard
#   /opt/etc/wolguard.conf          (registered as a conffile)
#   /opt/etc/init.d/S99wolguard
#
# Package layout (matches the current Entware feed, OpenWrt ipkg-build style):
#   wolguard_<version>_<arch>.ipk   gzip-compressed tar containing
#     ./debian-binary               "2.0\n"
#     ./data.tar.gz                 ./opt/... payload
#     ./control.tar.gz              ./control, ./conffiles, ./postinst, ./prerm
#
# On a Keenetic router the OPKG component also unpacks .ipk files placed in
# /opt/install (for example on a USB drive) automatically, so the package can
# be installed that way or explicitly with `opkg install <file>.ipk`.
#
# Usage: sh packaging/make-ipk.sh [OPTIONS] [BINARY] [OUTPUT_DIR]
#
#   BINARY      mipsel binary to package (default: dist/wolguard-mips)
#   OUTPUT_DIR  where the .ipk is written (default: dist)
#
# Options:
#   --ar        build the classic ar container (legacy opkg/dpkg tooling;
#               current Entware opkg expects the default tar.gz container)
#   --tar       build the tar.gz container (default)
#   --format F  same as --ar/--tar
#   -h, --help  show this help
#
# Environment:
#   VERSION            full package version, overrides Cargo.toml + REVISION
#   REVISION           package revision appended to the Cargo.toml version
#                      (default: 1, giving Version: 0.1.0-1)
#   ARCH               package architecture (default: mipsel-3.4)
#   DEPENDS            runtime dependencies (default: empty, the binary is
#                      static musl, so there is no libc dependency)
#   MAINTAINER         maintainer field
#   IPK_FORMAT         "tar" or "ar", same as --tar/--ar
#   SOURCE_DATE_EPOCH  fixed timestamp stored in both inner archives
#                      (default: 0); with sorted members and `gzip -n` this
#                      makes the build byte-for-byte reproducible
#
# Requirements: POSIX sh, GNU tar (--sort/--owner/--mtime), gzip; ar (binutils)
# is needed only for --ar.

set -u
export LC_ALL=C

PKG_NAME=wolguard
ARCH=${ARCH:-mipsel-3.4}
REVISION=${REVISION:-1}
MAINTAINER=${MAINTAINER:-wolguard maintainers}
DEPENDS=${DEPENDS:-}
FORMAT=${IPK_FORMAT:-tar}
MTIME=${SOURCE_DATE_EPOCH:-0}

ROOT=`CDPATH= cd "$(dirname "$0")/.." && pwd`

BIN=
OUT_DIR=
WORK=

die() {
	printf 'make-ipk: ERROR: %s\n' "$*" >&2
	exit 1
}

note() {
	printf 'make-ipk: %s\n' "$*"
}

usage() {
	cat <<EOF
Build an Entware .ipk package for wolguard.

Usage: sh packaging/make-ipk.sh [OPTIONS] [BINARY] [OUTPUT_DIR]

  BINARY      mipsel binary to package (default: $ROOT/dist/wolguard-mips)
  OUTPUT_DIR  where the .ipk is written (default: $ROOT/dist)

Options:
  --ar         build the classic ar container (legacy tooling)
  --tar        build the tar.gz container (default; current Entware opkg)
  --format F   same as --ar/--tar
  -h, --help   show this help

Environment:
  VERSION            full package version (default: Cargo.toml version + REVISION)
  REVISION           package revision (default: 1)
  ARCH               package architecture (default: mipsel-3.4)
  DEPENDS            runtime dependencies (default: empty, static binary)
  MAINTAINER         maintainer field
  IPK_FORMAT         "tar" or "ar", same as --tar/--ar
  SOURCE_DATE_EPOCH  fixed timestamp for reproducible builds (default: 0)

The result is e.g. $ROOT/dist/${PKG_NAME}_0.1.0-1_${ARCH}.ipk
EOF
}

need_cmd() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

cleanup() {
	[ -n "${WORK:-}" ] || return 0
	case "$WORK" in
	*/wolguard-ipk.*) rm -rf "$WORK" ;;
	esac
}

# Create a deterministic gzip-compressed tar of DIR, same options as OpenWrt's
# ipkg-build: GNU format, numeric 0:0 ownership, sorted member names (so file
# system order does not leak in) and a fixed timestamp. `gzip -n` drops the
# timestamp from the gzip header as well.
create_inner_tar() {
	_dir=$1
	_out=$2
	(
		cd "$_dir" || exit 1
		tar --format=gnu --numeric-owner --sort=name -cpf "$WORK/inner.tar" \
			--mtime="@$MTIME" --owner=0 --group=0 . || exit 1
	) || die "tar failed for $_dir"
	gzip -n -9 -c "$WORK/inner.tar" >"$_out" || die "gzip failed for $_out"
	rm -f "$WORK/inner.tar"
}

# Check the permission bits stored in the archive (not the extracted copy,
# whose modes are influenced by the local umask).
_check_mode() {
	_mode=`tar tvzf "$1" | awk -v member="$2" '$NF == member { print $1; exit }'`
	[ -n "$_mode" ] || die "cannot find $2 in $1"
	[ "$_mode" = "$3" ] || die "wrong mode for $4: expected $3, got $_mode"
}

# Verify the finished package the way opkg reads it: unpack the outer
# container, then check control fields, conffiles, member list and modes.
verify_package() {
	_pkg=$1
	rm -rf "$WORK/outer" "$WORK/data_files" "$WORK/control_files"
	mkdir -p "$WORK/outer" || die "cannot create $WORK/outer"
	case "$FORMAT" in
	ar) (cd "$WORK/outer" && ar x "$_pkg") || die "cannot unpack ar members from $_pkg" ;;
	tar) (cd "$WORK/outer" && tar xzf "$_pkg") || die "cannot unpack tar members from $_pkg" ;;
	esac

	[ -f "$WORK/outer/debian-binary" ] || die "package has no debian-binary member"
	[ "`cat "$WORK/outer/debian-binary"`" = "2.0" ] || die "debian-binary does not contain 2.0"
	[ -f "$WORK/outer/control.tar.gz" ] || die "package has no control.tar.gz member"
	[ -f "$WORK/outer/data.tar.gz" ] || die "package has no data.tar.gz member"

	tar tzf "$WORK/outer/control.tar.gz" >"$WORK/control.list" ||
		die "cannot list control.tar.gz"
	mkdir -p "$WORK/control_files" || die "cannot create $WORK/control_files"
	tar xzf "$WORK/outer/control.tar.gz" -C "$WORK/control_files" ||
		die "cannot extract control.tar.gz"

	for _member in ./control ./conffiles ./postinst ./prerm; do
		grep -qxF "$_member" "$WORK/control.list" ||
			die "control.tar.gz is missing $_member"
	done

	grep -qx "Package: $PKG_NAME" "$WORK/control_files/control" ||
		die "control is missing 'Package: $PKG_NAME'"
	grep -qx "Version: $VERSION" "$WORK/control_files/control" ||
		die "control is missing 'Version: $VERSION'"
	grep -qx "Architecture: $ARCH" "$WORK/control_files/control" ||
		die "control is missing 'Architecture: $ARCH'"
	grep -qx "Section: net" "$WORK/control_files/control" ||
		die "control is missing 'Section: net'"
	grep -qx "Priority: optional" "$WORK/control_files/control" ||
		die "control is missing 'Priority: optional'"
	grep -q '^Maintainer: ' "$WORK/control_files/control" ||
		die "control is missing 'Maintainer:'"
	grep -q '^Installed-Size: ' "$WORK/control_files/control" ||
		die "control is missing 'Installed-Size:'"
	grep -q '^Description: ' "$WORK/control_files/control" ||
		die "control is missing 'Description:'"
	if [ -n "$DEPENDS" ]; then
		grep -qx "Depends: $DEPENDS" "$WORK/control_files/control" ||
			die "control is missing 'Depends: $DEPENDS'"
	fi
	grep -qxF "/opt/etc/wolguard.conf" "$WORK/control_files/conffiles" ||
		die "conffiles does not list /opt/etc/wolguard.conf"

	# Installed-Size must be the size of the uncompressed data.tar stream.
	_size=`gzip -dc "$WORK/outer/data.tar.gz" | wc -c`
	_expected="Installed-Size: `printf '%s' "$_size" | tr -d ' '`"
	grep -qx "$_expected" "$WORK/control_files/control" ||
		die "control does not say '$_expected'"

	tar tzf "$WORK/outer/data.tar.gz" >"$WORK/data.list" ||
		die "cannot list data.tar.gz"
	mkdir -p "$WORK/data_files" || die "cannot create $WORK/data_files"
	tar -xpzf "$WORK/outer/data.tar.gz" -C "$WORK/data_files" ||
		die "cannot extract data.tar.gz"

	for _member in ./opt/bin/wolguard ./opt/etc/wolguard.conf ./opt/etc/init.d/S99wolguard; do
		grep -qxF "$_member" "$WORK/data.list" ||
			die "data.tar.gz is missing $_member"
	done

	if command -v awk >/dev/null 2>&1; then
		_check_mode "$WORK/outer/data.tar.gz" ./opt/bin/wolguard -rwxr-xr-x "binary"
		_check_mode "$WORK/outer/data.tar.gz" ./opt/etc/wolguard.conf -rw-r--r-- "config"
		_check_mode "$WORK/outer/data.tar.gz" ./opt/etc/init.d/S99wolguard -rwxr-xr-x "init script"
		_check_mode "$WORK/outer/control.tar.gz" ./control -rw-r--r-- "control"
		_check_mode "$WORK/outer/control.tar.gz" ./conffiles -rw-r--r-- "conffiles"
		_check_mode "$WORK/outer/control.tar.gz" ./postinst -rwxr-xr-x "postinst"
		_check_mode "$WORK/outer/control.tar.gz" ./prerm -rwxr-xr-x "prerm"
	fi

	printf '\n== package ==\n%s (%s bytes, %s container)\n' "$_pkg" "`wc -c <"$_pkg"`" "$FORMAT"
	case "$FORMAT" in
	ar)
		printf '\nar t:\n'
		ar t "$_pkg"
		;;
	tar)
		printf '\ntar tzf:\n'
		tar tzf "$_pkg"
		;;
	esac
	printf '\ncontrol.tar.gz:\n'
	tar tzf "$WORK/outer/control.tar.gz"
	printf '\ndata.tar.gz:\n'
	tar tzf "$WORK/outer/data.tar.gz"
	printf '\ncontrol:\n'
	cat "$WORK/control_files/control"
	printf '\nconffiles:\n'
	cat "$WORK/control_files/conffiles"
	printf '\n'
}

while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help)
		usage
		exit 0
		;;
	--ar) FORMAT=ar ;;
	--tar) FORMAT=tar ;;
	--format)
		[ $# -ge 2 ] || die "--format needs an argument (tar or ar)"
		FORMAT=$2
		shift
		;;
	--format=*) FORMAT=${1#--format=} ;;
	-*) die "unknown option: $1 (try --help)" ;;
	*)
		if [ -z "$BIN" ]; then
			BIN=$1
		elif [ -z "$OUT_DIR" ]; then
			OUT_DIR=$1
		else
			die "unexpected argument: $1"
		fi
		;;
	esac
	shift
done
if [ -z "$BIN" ]; then
	BIN=$ROOT/dist/wolguard-mips
fi
if [ -z "$OUT_DIR" ]; then
	OUT_DIR=$ROOT/dist
fi

case "$FORMAT" in
tar | ar) ;;
*) die "IPK_FORMAT must be tar or ar, got: $FORMAT" ;;
esac
case "$MTIME" in
'' | *[!0-9]*) die "SOURCE_DATE_EPOCH must be a non-negative integer, got: $MTIME" ;;
esac

if [ -z "${VERSION:-}" ]; then
	_upstream=
	if [ -f "$ROOT/Cargo.toml" ]; then
		_upstream=`awk '
			/^\[package\]/ { in_package = 1; next }
			/^\[/ { in_package = 0 }
			in_package && /^version[[:space:]]*=/ {
				split($0, parts, "\"")
				print parts[2]
				exit
			}
		' "$ROOT/Cargo.toml"`
	fi
	[ -n "$_upstream" ] || _upstream=0.1.0
	VERSION="${_upstream}-${REVISION}"
fi

case "$BIN" in
/*) ;;
*) BIN=`pwd`/$BIN ;;
esac
case "$OUT_DIR" in
/*) ;;
*) OUT_DIR=`pwd`/$OUT_DIR ;;
esac
OUT=$OUT_DIR/${PKG_NAME}_${VERSION}_${ARCH}.ipk

need_cmd tar
need_cmd gzip
need_cmd mktemp
if [ "$FORMAT" = "ar" ]; then
	need_cmd ar
fi
if ! tar --help 2>/dev/null | grep -q -- '--mtime'; then
	note "warning: this tar may not support --mtime/--owner, the archives may not be reproducible"
fi

[ -f "$BIN" ] || die "binary not found: $BIN (build it first with ./build-mips.sh)"
[ -s "$BIN" ] || die "binary is empty: $BIN"
SRC_CONF=$ROOT/packaging/wolguard.conf
SRC_INIT=$ROOT/packaging/S99wolguard
[ -f "$SRC_CONF" ] || die "default config not found: $SRC_CONF"
[ -f "$SRC_INIT" ] || die "init script not found: $SRC_INIT"

WORK=`mktemp -d "${TMPDIR:-/tmp}/wolguard-ipk.XXXXXX"` || die "cannot create temporary directory"
trap cleanup 0 1 2 15

note "packaging $PKG_NAME $VERSION for $ARCH ($FORMAT container)"
note "binary: $BIN"

DATA=$WORK/data
CTRL=$WORK/control
PKGDIR=$WORK/pkg
mkdir -p "$DATA/opt/bin" "$DATA/opt/etc/init.d" "$CTRL" "$PKGDIR" ||
	die "cannot create staging directories in $WORK"
# Archive directory entries must not carry the local umask: opkg applies the
# stored mode when it unpacks, and "./opt" with mode 700 would be wrong.
chmod 755 "$DATA" "$DATA/opt" "$DATA/opt/bin" "$DATA/opt/etc" \
	"$DATA/opt/etc/init.d" "$CTRL" || die "cannot chmod staging directories"

cp "$BIN" "$DATA/opt/bin/wolguard" || die "cannot copy $BIN"
chmod 755 "$DATA/opt/bin/wolguard" || die "cannot chmod the binary"
cp "$SRC_CONF" "$DATA/opt/etc/wolguard.conf" || die "cannot copy $SRC_CONF"
chmod 644 "$DATA/opt/etc/wolguard.conf" || die "cannot chmod the config"
cp "$SRC_INIT" "$DATA/opt/etc/init.d/S99wolguard" || die "cannot copy $SRC_INIT"
chmod 755 "$DATA/opt/etc/init.d/S99wolguard" || die "cannot chmod the init script"

# Installed-Size is the size of the uncompressed data.tar stream, as
# OpenWrt's ipkg-build does it (not the sum of the file sizes).
create_inner_tar "$DATA" "$WORK/data.tar.gz"
INSTALLED_SIZE=`gzip -dc "$WORK/data.tar.gz" | wc -c | tr -d ' '`
[ -n "$INSTALLED_SIZE" ] || INSTALLED_SIZE=0

{
	printf 'Package: %s\n' "$PKG_NAME"
	printf 'Version: %s\n' "$VERSION"
	if [ -n "$DEPENDS" ]; then
		printf 'Depends: %s\n' "$DEPENDS"
	fi
	printf 'Section: net\n'
	printf 'Priority: optional\n'
	printf 'Maintainer: %s\n' "$MAINTAINER"
	printf 'Architecture: %s\n' "$ARCH"
	printf 'Installed-Size: %s\n' "$INSTALLED_SIZE"
	printf 'Description: Wake-on-LAN guard daemon for Keenetic/Entware routers\n'
	printf ' Probes the configured LAN hosts with ICMP and sends Wake-on-LAN\n'
	printf ' magic packets when a host stops answering. Includes a terminal\n'
	printf ' dashboard (wolguard tui) and config validation (check-config).\n'
} >"$CTRL/control" || die "cannot write control"
printf '/opt/etc/wolguard.conf\n' >"$CTRL/conffiles" || die "cannot write conffiles"

cat >"$CTRL/postinst" <<'EOF' || die "cannot write postinst"
#!/bin/sh
#
# Create runtime directories and start the service. A failure to start is
# reported but never fails the installation: the daemon needs a valid config,
# and a half-installed package is worse than a stopped service.
# On upgrade opkg runs prerm first, so this starts the new binary.

mkdir -p /opt/var/log /opt/var/run 2>/dev/null

if [ -x /opt/etc/init.d/S99wolguard ]; then
	/opt/etc/init.d/S99wolguard start >/dev/null 2>&1 ||
		echo "wolguard: service did not start, check /opt/var/log/wolguard.log" >&2
fi

exit 0
EOF

cat >"$CTRL/prerm" <<'EOF' || die "cannot write prerm"
#!/bin/sh
#
# Stop the daemon before the package files are removed.

if [ -x /opt/etc/init.d/S99wolguard ]; then
	/opt/etc/init.d/S99wolguard stop >/dev/null 2>&1 || true
fi

exit 0
EOF

chmod 644 "$CTRL/control" "$CTRL/conffiles" || die "cannot chmod control files"
chmod 755 "$CTRL/postinst" "$CTRL/prerm" || die "cannot chmod control scripts"

create_inner_tar "$CTRL" "$WORK/control.tar.gz"

printf '2.0\n' >"$PKGDIR/debian-binary" || die "cannot write debian-binary"
cp "$WORK/control.tar.gz" "$PKGDIR/control.tar.gz" || die "cannot stage control.tar.gz"
cp "$WORK/data.tar.gz" "$PKGDIR/data.tar.gz" || die "cannot stage data.tar.gz"
# Outer members are plain metadata files: force 0644 so that the result does
# not depend on the caller's umask.
chmod 644 "$PKGDIR/debian-binary" "$PKGDIR/control.tar.gz" "$PKGDIR/data.tar.gz" ||
	die "cannot chmod package members"

mkdir -p "$OUT_DIR" || die "cannot create output directory: $OUT_DIR"
rm -f "$OUT" || die "cannot remove old package: $OUT"

case "$FORMAT" in
ar)
	(cd "$PKGDIR" && ar rc "$OUT" debian-binary control.tar.gz data.tar.gz) ||
		die "ar failed to create $OUT"
	;;
tar)
	(
		cd "$PKGDIR" || exit 1
		tar --format=gnu --owner=0 --group=0 --numeric-owner --mtime="@$MTIME" \
			-cpf "$WORK/outer.tar" ./debian-binary ./data.tar.gz ./control.tar.gz ||
			exit 1
	) || die "tar failed to create the outer archive"
	gzip -n -9 -c "$WORK/outer.tar" >"$OUT" || die "gzip failed for $OUT"
	;;
esac

verify_package "$OUT"
chmod 644 "$OUT" 2>/dev/null || true

note "OK: $OUT"
if [ -z "$DEPENDS" ]; then
	note "no Depends field: the binary is statically linked (override with DEPENDS=...)"
fi
note "install on the router: opkg install $OUT"
note "or copy it to /opt/install on the router, OPKG unpacks it from there"
