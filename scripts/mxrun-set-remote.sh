#!/usr/bin/env sh
set -eu

command -v "${MXRUN_BIN:-mxrun}" >/dev/null 2>&1 || { echo "Missing ${MXRUN_BIN:-mxrun} binary. Install it first." >&2; exit 1; }
[ -f mxrun.conf ] || { echo "No mxrun.conf found in this project. Create one first." >&2; exit 1; }

printf 'active=yes\n' > .mxrun-env
printf '\nmxrun builds enabled. To switch back to local-only:\n    make set-local-builds\n\n'
