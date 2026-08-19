.DEFAULT_GOAL := help
.PHONY: help ensure-toolchain mxrun mxrun-init mxrun-toggle set-local-builds set-remote-builds dev worker-dev release worker check test integration-test setup image install-image prepare config docs docs-dev docs-clean musl-vscomm worker-netbsd clean
.PHONY: _dev _worker-dev _release _worker _check _test _integration-test

DOCS_VENV := .venv-docs
DOCS_MKDOCS := $(DOCS_VENV)/bin/mkdocs
VSCOMM_TARGET := x86_64-unknown-linux-musl
WORKER_TARGET ?= x86_64-unknown-netbsd
IMAGE ?=
OCI ?=
MXRUN_BIN ?= mxrun
MXRUN_ARGS ?=
MX_ACTIVE := $(shell awk -F= '/^active=/ {print $$2}' .mxrun-env 2>/dev/null)
export MXRUN_ARGS
export MXRUN_BIN
C_TITLE := \033[1;38;2;215;0;175m
C_COMMAND := \033[38;2;175;255;215m
C_DESCRIPTION := \033[38;2;128;128;128m
C_OFF := \033[0m

help:
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Development"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "dev" "Build all binaries (host + musl-static vscomm)"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "worker-dev" "Build only the remote worker (development)"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "check" "Format and lint"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Release"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "release" "Build optimized release binaries"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "worker" "Build only the remote worker (release)"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Testing"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "test" "Run tests (requires cargo-nextest)"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "integration-test" "Run sandbox integration tests"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Toolchain"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "ensure-toolchain" "Install/update Rust stable and musl target"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "musl-vscomm" "Build static vscomm binary only"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "worker-netbsd" "Build the portable worker for NetBSD"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Image"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "image" "Build OCI agent image (requires IMAGE=)"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "install-image" "Install OCI archive (requires OCI=)"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Setup"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "setup" "Install containerd, CNI, Kata dependencies"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "prepare" "Prepare workspace overlay layers"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "config" "Configure project interactively"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Documentation"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "docs" "Build documentation site"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "docs-dev" "Serve docs locally with live reload"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "docs-clean" "Remove docs build artifacts"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Cleanup"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "clean" "Remove build artifacts (cargo clean)"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "Utils"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "help" "Show this help"
	@printf '\n'
	@printf '$(C_TITLE)%s$(C_OFF)\n' "mxrun"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "mxrun" "Show mxrun status"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "mxrun-init" "Initialise mxrun with a local target"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "mxrun-toggle" "Toggle mxrun delegation"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "set-local-builds" "Disable mxrun delegation"
	@printf '  $(C_COMMAND)%-20s$(C_OFF) $(C_DESCRIPTION)%s$(C_OFF)\n' "set-remote-builds" "Enable mxrun delegation"
	@printf '\n'
	@if [ "$(MX_ACTIVE)" = "yes" ]; then \
		printf "$(C_OFF)mxrun enabled; builds use the configured target matrix.$(C_OFF)\n"; \
	else \
		printf "$(C_OFF)mxrun disabled; builds run through local targets.$(C_OFF)\n"; \
	fi

ensure-toolchain:
	@command -v rustup >/dev/null 2>&1 || { echo "rustup is required: https://rustup.rs" >&2; exit 1; }
	rustup update stable
	rustup target add $(VSCOMM_TARGET)

dev:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _dev; else scripts/maybe-mxrun.sh dev || $(MAKE) _dev; fi

_dev: ensure-toolchain
	cargo build --bin bunkerbox --bin bunkerbox-image
	cargo build -p bunkerbox-worker
	cargo build --bin bunkerbox-netrelay --bin bunkerbox-vscomm --bin bunkerbox-remote --target $(VSCOMM_TARGET)
	cargo build --bin bunkerbox-status --target $(VSCOMM_TARGET)
	rm -rf target/dist
	mkdir -p target/dist
	cp target/debug/bunkerbox target/dist/
	cp target/debug/bunkerbox-image target/dist/
	cp target/$(VSCOMM_TARGET)/debug/bunkerbox-netrelay target/dist/
	cp target/$(VSCOMM_TARGET)/debug/bunkerbox-vscomm target/dist/
	cp target/$(VSCOMM_TARGET)/debug/bunkerbox-remote target/dist/
	cp target/$(VSCOMM_TARGET)/debug/bunkerbox-status target/dist/
	cp target/$(VSCOMM_TARGET)/debug/bunkerbox-netrelay target/debug/bunkerbox-netrelay

worker-dev:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _worker-dev; else scripts/maybe-mxrun.sh worker-dev || $(MAKE) _worker-dev; fi

_worker-dev:
	cargo build -p bunkerbox-worker

release:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _release; else scripts/maybe-mxrun.sh release || $(MAKE) _release; fi

_release: ensure-toolchain
	cargo build --bin bunkerbox --bin bunkerbox-image --release
	cargo build --bin bunkerbox-netrelay --bin bunkerbox-vscomm --bin bunkerbox-remote --target $(VSCOMM_TARGET) --release
	cargo build --bin bunkerbox-status --target $(VSCOMM_TARGET) --release
	rm -rf target/dist
	mkdir -p target/dist
	cp target/release/bunkerbox target/dist/
	cp target/release/bunkerbox-image target/dist/
	cp target/$(VSCOMM_TARGET)/release/bunkerbox-netrelay target/dist/
	cp target/$(VSCOMM_TARGET)/release/bunkerbox-vscomm target/dist/
	cp target/$(VSCOMM_TARGET)/release/bunkerbox-remote target/dist/
	cp target/$(VSCOMM_TARGET)/release/bunkerbox-status target/dist/
	cp target/$(VSCOMM_TARGET)/release/bunkerbox-netrelay target/release/bunkerbox-netrelay

worker:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _worker; else scripts/maybe-mxrun.sh worker || $(MAKE) _worker; fi

_worker:
	cargo build -p bunkerbox-worker --release

check:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _check; else scripts/maybe-mxrun.sh check || $(MAKE) _check; fi

_check:
	cargo fmt --all
	cargo clippy --all-targets --all-features -- -D warnings || cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings

test:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _test; else scripts/maybe-mxrun.sh test || $(MAKE) _test; fi

_test:
	cargo nextest run

integration-test:
	@if [ -n "$$SSH_CONNECTION" ]; then $(MAKE) _integration-test; else scripts/maybe-mxrun.sh integration-test || $(MAKE) _integration-test; fi

_integration-test: _dev
	cargo nextest run --test test_base --test test_sandbox

mxrun-toggle:
	@if [ -f .mxrun-env ] && grep -q '^active=yes' .mxrun-env 2>/dev/null; then \
		sh scripts/mxrun-set-local.sh; \
	else \
		sh scripts/mxrun-set-remote.sh; \
	fi

set-local-builds:
	sh scripts/mxrun-set-local.sh

set-remote-builds:
	sh scripts/mxrun-set-remote.sh

mxrun-init:
	@command -v $(MXRUN_BIN) >/dev/null 2>&1 || { echo "Missing $(MXRUN_BIN). Install it first." >&2; exit 1; }
	@if [ ! -f mxrun.conf ]; then printf 'local\n' > mxrun.conf; fi
	@printf 'active=yes\n' > .mxrun-env
	@MXRUN_CONFIG=mxrun.conf MXRUN_LOCAL_MAKE='$(MAKE)' $(MXRUN_BIN) init || true

mxrun:
	@command -v $(MXRUN_BIN) >/dev/null 2>&1 || { echo "Missing $(MXRUN_BIN). Install it first." >&2; exit 1; }
	@if [ ! -f mxrun.conf ] && [ ! -f .mxrun-env ]; then printf 'active=no\n' > .mxrun-env; fi
	@sh scripts/mxrun-status.sh

setup: dev
	target/debug/bunkerbox setup

musl-vscomm: ensure-toolchain
	cargo build --bin bunkerbox-vscomm --bin bunkerbox-remote --target $(VSCOMM_TARGET)
	cargo build --bin bunkerbox-status --target $(VSCOMM_TARGET)

worker-netbsd:
	cargo build -p bunkerbox-worker --target $(WORKER_TARGET) --release

image: dev
	@if [ -z "$(IMAGE)" ]; then echo "usage: make image IMAGE=images/name.conf" >&2; exit 1; fi
	target/debug/bunkerbox-image $(IMAGE)

install-image: dev
	@if [ -z "$(OCI)" ]; then echo "usage: make install-image OCI=path/to/image.oci" >&2; exit 1; fi
	BUNKERBOX_OCI_ARCHIVE=$(OCI) target/debug/bunkerbox install-image

prepare: dev
	target/debug/bunkerbox prepare

config: dev
	target/debug/bunkerbox config

$(DOCS_MKDOCS): docs/requirements.txt
	python3 -m venv $(DOCS_VENV)
	$(DOCS_VENV)/bin/pip install -r docs/requirements.txt

docs: $(DOCS_MKDOCS)
	$(DOCS_MKDOCS) build --strict

docs-dev: $(DOCS_MKDOCS)
	$(DOCS_MKDOCS) serve

docs-clean:
	rm -rf target/site-docs $(DOCS_VENV)

clean:
	cargo clean
