.DEFAULT_GOAL := help
.PHONY: help ensure-toolchain dev release check test integration-test setup image install-image prepare config docs docs-clean musl-vscomm

DOCS_VENV := .venv-docs
DOCS_MKDOCS := $(DOCS_VENV)/bin/mkdocs
VSCOMM_TARGET := x86_64-unknown-linux-musl
IMAGE ?=
OCI ?=

help:
	@printf "  %-24s %s\n"    "Development" ""
	@printf "  %-24s %s\n"    "  dev" "Build all binaries (host + musl-static vscomm)"
	@printf "  %-24s %s\n"    "  release" "Build optimized release binaries"
	@printf "  %-24s %s\n"    "  check" "Format and lint"
	@printf "  %-24s %s\n"    "  test" "Run tests (requires cargo-nextest)"
	@printf "  %-24s %s\n"    "  integration-test" "Run sandbox integration tests"
	@printf "  %-24s %s\n"    "  docs" "Build documentation site"
	@printf "  %-24s %s\n"    "" ""
	@printf "  %-24s %s\n"    "Toolchain" ""
	@printf "  %-24s %s\n"    "  ensure-toolchain" "Install/update Rust stable and musl target"
	@printf "  %-24s %s\n"    "  musl-vscomm" "Build static vscomm binary only"
	@printf "  %-24s %s\n"    "" ""
	@printf "  %-24s %s\n"    "Image" ""
	@printf "  %-24s %s\n"    "  image" "Build OCI agent image (requires IMAGE=)"
	@printf "  %-24s %s\n"    "  install-image" "Install OCI archive (requires OCI=)"
	@printf "  %-24s %s\n"    "" ""
	@printf "  %-24s %s\n"    "Setup" ""
	@printf "  %-24s %s\n"    "  setup" "Install containerd, CNI, Kata dependencies"
	@printf "  %-24s %s\n"    "  prepare" "Prepare workspace overlay layers"
	@printf "  %-24s %s\n"    "  config" "Configure project interactively"
	@printf "  %-24s %s\n"    "" ""
	@printf "  %-24s %s\n"    "Cleanup" ""
	@printf "  %-24s %s\n"    "  docs-clean" "Remove docs build artifacts"

ensure-toolchain:
	@command -v rustup >/dev/null 2>&1 || { echo "rustup is required: https://rustup.rs" >&2; exit 1; }
	rustup update stable
	rustup target add $(VSCOMM_TARGET)

dev: ensure-toolchain
	cargo build
	cargo build --bin bunkerbox-vscomm --target $(VSCOMM_TARGET)

release: ensure-toolchain
	cargo build --release
	cargo build --bin bunkerbox-vscomm --target $(VSCOMM_TARGET) --release

check:
	cargo fmt --all
	cargo clippy --all-targets --all-features -- -D warnings || cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings

test:
	cargo nextest run

integration-test: dev
	cargo nextest run --test test_base --test test_sandbox

setup: dev
	target/debug/bunkerbox setup

musl-vscomm: ensure-toolchain
	cargo build --bin bunkerbox-vscomm --target $(VSCOMM_TARGET)

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

docs-clean:
	rm -rf target/site-docs $(DOCS_VENV)
