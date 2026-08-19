#!/usr/bin/env sh
set -eu

ENTRY="${1:-}"
[ -n "$ENTRY" ] || { echo "maybe-mxrun.sh: missing entry argument" >&2; exit 1; }

# mxrun clears MXRUN_LOCAL_MAKE when calling back into Make. The callback must
# run the private local target instead of recursively delegating.
[ "${MXRUN_LOCAL_MAKE+set}" = "set" ] && exit 1

ACTIVE=no
[ -f .mxrun-env ] && ACTIVE=$(awk -F= '/^active=/ {print $2}' .mxrun-env 2>/dev/null)

if [ "$ACTIVE" = "yes" ] && [ -f mxrun.conf ]; then
	command -v "${MXRUN_BIN:-mxrun}" >/dev/null 2>&1 || { echo "Missing ${MXRUN_BIN:-mxrun} binary. Install it first." >&2; exit 1; }
	case "$ENTRY" in
		dev|worker-dev|check)
			LABEL="Development Build Mode" ;;
		release|worker)
			LABEL="Release Build Mode" ;;
		test)
			LABEL="Testing Everything" ;;
		integration-test)
			LABEL="Integration Tests" ;;
		*)
			LABEL="Bunkerbox Build" ;;
	esac
	# shellcheck disable=SC2086
	case "$ENTRY" in
		worker-dev|worker)
			"${MXRUN_BIN:-mxrun}" --config remote-mxrun.conf run --label="$LABEL" ${MXRUN_ARGS:-} "$ENTRY" || true ;;
		*)
			MXRUN_CONFIG=mxrun.conf "${MXRUN_BIN:-mxrun}" run --label="$LABEL" ${MXRUN_ARGS:-} "$ENTRY" || true ;;
	esac
	# mxrun handled the request or was interrupted; do not fall through locally.
	exit 0
fi

# mxrun is inactive; the caller falls back to its private local target.
exit 1
