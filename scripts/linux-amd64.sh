#!/usr/bin/env sh
set -eu

ACTION="${1:-}"
CARGO="${CARGO:-cargo}"

require_linker() {
	command -v cc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || {
		echo "Linux worker setup requires a C compiler (cc, clang, or gcc)." >&2
		echo "Install one as root, then rerun the worker build." >&2
		exit 1
	}
}

setup() {
	[ -n "${HOME:-}" ] || {
		echo "Linux worker setup requires HOME for a user-local Rust toolchain" >&2
		exit 1
	}

	export PATH="$HOME/.cargo/bin:$PATH"
	if command -v "$CARGO" >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
		require_linker
		return
	fi

	if [ "$(id -u)" -eq 0 ]; then
		echo "Linux worker setup needs Rust, but this process is root." >&2
		echo "Install cargo and rustc system-wide, or rerun as a non-root user." >&2
		exit 1
	fi

	if ! command -v rustup >/dev/null 2>&1; then
		rustup_script=$(mktemp "${TMPDIR:-/tmp}/bunkerbox-rustup.XXXXXX")
		cleanup() { rm -f "$rustup_script"; }
		trap cleanup 0 1 2 15

		if command -v curl >/dev/null 2>&1; then
			curl --fail --silent --show-error --location https://sh.rustup.rs > "$rustup_script"
		elif command -v fetch >/dev/null 2>&1; then
			fetch -o "$rustup_script" https://sh.rustup.rs
		elif command -v wget >/dev/null 2>&1; then
			wget -qO "$rustup_script" https://sh.rustup.rs
		else
			echo "Linux worker setup requires curl, fetch, or wget." >&2
			echo "Install one as root, then rerun the worker build." >&2
			exit 1
		fi

		if ! sh "$rustup_script" -y --profile minimal --default-toolchain stable --no-modify-path; then
			echo "Linux worker setup could not install Rust user-locally." >&2
			echo "Install cargo and rustc as root, or fix the user-local rustup setup." >&2
			exit 1
		fi
	fi

	export PATH="$HOME/.cargo/bin:$PATH"
	if ! rustup toolchain install stable --profile minimal; then
		echo "Linux worker setup could not install the stable Rust toolchain." >&2
		exit 1
	fi
	if ! rustup default stable; then
		echo "Linux worker setup could not select the stable Rust toolchain." >&2
		exit 1
	fi

	command -v "$CARGO" >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1 || {
		echo "Linux worker setup could not provide cargo and rustc." >&2
		exit 1
	}
	require_linker
}

setup

case "$ACTION" in
	worker-dev)
		exec "$CARGO" build -p bunkerbox-worker ;;
	worker)
		exec "$CARGO" build -p bunkerbox-worker --release ;;
	*)
		echo "Unsupported Linux worker action: $ACTION" >&2
		exit 1 ;;
esac
