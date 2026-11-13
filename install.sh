#!/bin/sh
#
# install.sh - install wolguard on a Keenetic router running Entware.
#
# Run this script on the router, not on the development machine:
#
#   sh install.sh [BINARY]
#
# It copies the binary to /opt/bin/wolguard, installs the default
# /opt/etc/wolguard.conf (only when no config exists yet) and the init script
# /opt/etc/init.d/S99wolguard, validates the config and starts the service.
#
# The script expects packaging/wolguard.conf and packaging/S99wolguard next to
# it (or next to its parent, when it is executed from the project root).
#
# Usage: sh install.sh [OPTIONS] [BINARY]
#
#   BINARY      mipsel binary to install
#               (default: ./wolguard-mips, ./wolguard, ./dist/wolguard-mips)
#
# Options:
#   --no-start   install the files but leave the service stopped
#   --force      install even when the binary does not look like MIPS ELF32
#   --uninstall  stop the service, remove /opt/bin/wolguard and the init
#                script, keep /opt/etc/wolguard.conf
#   --purge      like --uninstall, also remove config, log and pid file
#   -h, --help   show this help
#
# Exit codes:
#   0 success
#   1 usage error
#   2 environment error (not root, missing or read-only /opt)
#   3 unusable binary
#   4 install/uninstall step failed
#   5 configuration is invalid
#   6 service failed to start

set -u

PROG=install.sh
BIN_DST=/opt/bin/wolguard
CONFIG=/opt/etc/wolguard.conf
INIT=/opt/etc/init.d/S99wolguard
LOGFILE=/opt/var/log/wolguard.log
PIDFILE=/opt/var/run/wolguard.pid

ACTION=install
NO_START=no
FORCE=no
BIN_ARG=
SCRIPT_DIR=`CDPATH= cd "$(dirname "$0")" && pwd`

info() {
	printf '%s: %s\n' "$PROG" "$*"
}

warn() {
	printf '%s: WARNING: %s\n' "$PROG" "$*" >&2
}

error() {
	printf '%s: ERROR: %s\n' "$PROG" "$*" >&2
}

usage() {
	cat <<EOF
Install wolguard on a Keenetic/Entware router.

Usage: sh $PROG [OPTIONS] [BINARY]

  BINARY      mipsel binary to install
              (default: ./wolguard-mips, ./wolguard, ./dist/wolguard-mips)

Options:
  --no-start   install the files but do not start or restart the service
  --force      install even if the binary does not look like MIPS ELF32
  --uninstall  stop the service and remove the binary and init script,
               keep $CONFIG
  --purge      like --uninstall, also remove the config, log and pid file
  -h, --help   show this help

Files installed:
  $BIN_DST
  $CONFIG   (only when it does not exist yet)
  $INIT

Exit codes:
  0 success, 1 usage, 2 environment, 3 bad binary, 4 install failure,
  5 invalid config, 6 service failed to start
EOF
}

require_root() {
	_uid=`id -u 2>/dev/null`
	if [ "$_uid" != "0" ]; then
		error "must be run as root (a Keenetic Entware shell is already root)"
		exit 2
	fi
}

require_opt() {
	if [ ! -d /opt ]; then
		error "/opt is missing - is Entware installed and mounted?"
		exit 2
	fi
	if [ ! -w /opt ]; then
		error "/opt is not writable - check the Entware partition"
		exit 2
	fi
}

find_src() {
	# find_src FILE...: print the first existing file, return 1 when none does
	for _candidate in "$@"; do
		if [ -f "$_candidate" ]; then
			printf '%s\n' "$_candidate"
			return 0
		fi
	done
	return 1
}

# Check that FILE is a 32-bit little-endian MIPS ELF executable.
# Returns 0 = looks like MIPS, 1 = wrong architecture, 2 = cannot tell.
check_binary_arch() {
	_arch_file=$1
	command -v od >/dev/null 2>&1 || return 2
	# ELF magic: 0x7f 'E' 'L' 'F'
	set -- `od -An -tu1 -j0 -N4 "$_arch_file" 2>/dev/null`
	[ $# -eq 4 ] || return 2
	[ "$1" -eq 127 ] && [ "$2" -eq 69 ] && [ "$3" -eq 76 ] && [ "$4" -eq 70 ] || return 1
	# e_ident[EI_CLASS] = 1 (ELF32), e_ident[EI_DATA] = 1 (little endian)
	set -- `od -An -tu1 -j4 -N2 "$_arch_file" 2>/dev/null`
	[ $# -eq 2 ] || return 2
	[ "$1" -eq 1 ] && [ "$2" -eq 1 ] || return 1
	# e_machine = 0x0008 (EM_MIPS) stored little endian
	set -- `od -An -tu1 -j18 -N2 "$_arch_file" 2>/dev/null`
	[ $# -eq 2 ] || return 2
	[ "$1" -eq 8 ] && [ "$2" -eq 0 ] || return 1
	return 0
}

resolve_binary() {
	if [ -n "$BIN_ARG" ]; then
		BIN_SRC=$BIN_ARG
	else
		BIN_SRC=`find_src "$SCRIPT_DIR/wolguard-mips" "$SCRIPT_DIR/wolguard" \
			"$SCRIPT_DIR/dist/wolguard-mips"` || BIN_SRC=
	fi
	if [ -z "$BIN_SRC" ]; then
		error "no binary given and none of wolguard-mips, wolguard, dist/wolguard-mips exists next to $PROG"
		error "usage: sh $PROG [BINARY]"
		exit 1
	fi
	if [ ! -f "$BIN_SRC" ]; then
		error "binary not found: $BIN_SRC"
		exit 3
	fi
	if [ ! -s "$BIN_SRC" ]; then
		error "binary is empty: $BIN_SRC"
		exit 3
	fi
	check_binary_arch "$BIN_SRC"
	case $? in
	0)
		info "binary check: MIPS ELF32 little-endian"
		;;
	1)
		if [ "$FORCE" = "yes" ]; then
			warn "$BIN_SRC does not look like a MIPS ELF32 binary, continuing because of --force"
		else
			error "$BIN_SRC is not a MIPS ELF32 little-endian binary"
			error "build it for mipsel-unknown-linux-musl or re-run with --force to install anyway"
			exit 3
		fi
		;;
	*)
		warn "cannot inspect $BIN_SRC (od is missing?), architecture not verified"
		;;
	esac
}

install_files() {
	CONF_SRC=`find_src "$SCRIPT_DIR/packaging/wolguard.conf" \
		"$SCRIPT_DIR/wolguard.conf" "$SCRIPT_DIR/dist/wolguard.conf"` ||
		{ error "default config not found next to $PROG"; error "copy packaging/wolguard.conf together with $PROG"; exit 4; }
	INIT_SRC=`find_src "$SCRIPT_DIR/packaging/S99wolguard" \
		"$SCRIPT_DIR/S99wolguard" "$SCRIPT_DIR/dist/S99wolguard"` ||
		{ error "init script not found next to $PROG"; error "copy packaging/S99wolguard together with $PROG"; exit 4; }

	WAS_RUNNING=no
	if [ -x "$INIT" ] && "$INIT" status >/dev/null 2>&1; then
		WAS_RUNNING=yes
	fi

	for _dir in /opt/bin /opt/etc/init.d /opt/var/log /opt/var/run; do
		mkdir -p "$_dir" 2>/dev/null || { error "cannot create $_dir"; exit 4; }
	done

	if [ -f "$BIN_DST" ]; then
		if cp -p "$BIN_DST" "$BIN_DST.bak" 2>/dev/null; then
			info "previous binary saved as $BIN_DST.bak"
		else
			warn "cannot back up $BIN_DST, continuing"
		fi
	fi
	if ! cp "$BIN_SRC" "$BIN_DST.new" 2>/dev/null; then
		error "cannot copy $BIN_SRC to $BIN_DST.new (out of space?)"
		rm -f "$BIN_DST.new" 2>/dev/null
		exit 4
	fi
	if ! chmod 755 "$BIN_DST.new" 2>/dev/null; then
		error "cannot chmod $BIN_DST.new"
		rm -f "$BIN_DST.new" 2>/dev/null
		exit 4
	fi
	if ! mv -f "$BIN_DST.new" "$BIN_DST" 2>/dev/null; then
		error "cannot install $BIN_DST"
		rm -f "$BIN_DST.new" 2>/dev/null
		exit 4
	fi
	info "installed $BIN_DST"

	if [ -f "$CONFIG" ]; then
		info "keeping existing $CONFIG"
	else
		if ! cp "$CONF_SRC" "$CONFIG" 2>/dev/null || ! chmod 644 "$CONFIG" 2>/dev/null; then
			error "cannot install $CONFIG"
			exit 4
		fi
		info "installed default $CONFIG (edit the [[host]] MAC before use)"
	fi

	if ! cp "$INIT_SRC" "$INIT" 2>/dev/null || ! chmod 755 "$INIT" 2>/dev/null; then
		error "cannot install $INIT"
		exit 4
	fi
	info "installed $INIT"

	info "validating configuration"
	if ! "$BIN_DST" --config "$CONFIG" check-config; then
		error "configuration is invalid: $CONFIG"
		error "fix it, then start the service: $INIT start"
		exit 5
	fi
	info "configuration is valid"

	if [ "$NO_START" = "yes" ]; then
		info "service not started (--no-start)"
		if [ "$WAS_RUNNING" = "yes" ]; then
			warn "the previously running daemon is still using the old binary"
		fi
		return 0
	fi
	if [ "$WAS_RUNNING" = "yes" ]; then
		info "restarting the service to load the new binary"
		if ! "$INIT" restart; then
			error "service failed to restart, see $LOGFILE"
			exit 6
		fi
	else
		info "starting the service"
		if ! "$INIT" start; then
			error "service failed to start, see $LOGFILE"
			exit 6
		fi
	fi
	"$INIT" status || true
}

print_next_steps() {
	cat <<EOF

wolguard is installed.

Next steps:
  1. edit $CONFIG: replace the example host and its MAC address
  2. validate it:  $BIN_DST --config $CONFIG check-config
  3. wake a host:  $BIN_DST wake server
  4. dashboard:    $BIN_DST tui
  5. status:       $INIT status
     logs:         tail -f $LOGFILE

The service starts automatically at boot (Entware rc.unslung runs S99wolguard
after the network is up).
EOF
}

stop_service() {
	if [ -x "$INIT" ]; then
		if "$INIT" stop >/dev/null 2>&1; then
			info "service stopped"
		else
			warn "service did not stop cleanly"
		fi
	elif [ -f "$PIDFILE" ]; then
		_pid=
		read -r _pid <"$PIDFILE" 2>/dev/null || _pid=
		case "$_pid" in
		'' | *[!0-9]*) _pid= ;;
		esac
		if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null; then
			kill "$_pid" 2>/dev/null && info "stopped pid $_pid from $PIDFILE"
		fi
	fi
}

remove_file() {
	[ -e "$1" ] || return 0
	if rm -f "$1" 2>/dev/null; then
		info "removed $1"
	else
		error "cannot remove $1"
		REMOVE_FAILED=1
	fi
}

uninstall() {
	REMOVE_FAILED=0
	stop_service
	remove_file "$PIDFILE"
	remove_file "$BIN_DST"
	remove_file "$BIN_DST.bak"
	remove_file "$INIT"
	if [ "$ACTION" = "purge" ]; then
		remove_file "$CONFIG"
		remove_file "$LOGFILE"
		info "purged configuration and log"
	else
		info "kept $CONFIG (use --purge to remove it too)"
	fi
	if [ "$REMOVE_FAILED" -ne 0 ]; then
		exit 4
	fi
	info "wolguard removed"
}

while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help)
		usage
		exit 0
		;;
	--no-start) NO_START=yes ;;
	--force) FORCE=yes ;;
	--uninstall)
		[ "$ACTION" = "purge" ] || ACTION=uninstall
		;;
	--purge) ACTION=purge ;;
	--)
		shift
		while [ $# -gt 0 ]; do
			if [ -n "$BIN_ARG" ]; then
				error "unexpected argument: $1"
				exit 1
			fi
			BIN_ARG=$1
			shift
		done
		break
		;;
	-*)
		error "unknown option: $1"
		usage >&2
		exit 1
		;;
	*)
		if [ -n "$BIN_ARG" ]; then
			error "unexpected argument: $1"
			exit 1
		fi
		BIN_ARG=$1
		;;
	esac
	shift
done

require_root
require_opt

if [ "$ACTION" = "uninstall" ] || [ "$ACTION" = "purge" ]; then
	uninstall
	exit 0
fi

resolve_binary
install_files
print_next_steps
