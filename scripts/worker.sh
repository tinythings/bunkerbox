#!/usr/bin/env sh
set -eu

ACTION="${1:-}"
[ -n "$ACTION" ] || { echo "worker.sh: missing worker action" >&2; exit 1; }

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS:$ARCH" in
	Linux:x86_64|Linux:amd64)
		PLATFORM=linux-amd64 ;;
	NetBSD:x86_64|NetBSD:amd64)
		PLATFORM=netbsd-amd64 ;;
	*)
		echo "Unsupported worker platform: OS=$OS ARCH=$ARCH" >&2
		exit 1 ;;
esac

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PLATFORM_SCRIPT="$SCRIPT_DIR/$PLATFORM.sh"

[ -x "$PLATFORM_SCRIPT" ] || {
	echo "Missing worker platform script: $PLATFORM_SCRIPT" >&2
	exit 1
}

exec "$PLATFORM_SCRIPT" "$ACTION"
