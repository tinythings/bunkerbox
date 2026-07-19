.PHONY: dev check test setup image install-image prepare config docs docs-clean musl-vscomm

DOCS_VENV := .venv-docs
DOCS_MKDOCS := $(DOCS_VENV)/bin/mkdocs
VSCOMM_TARGET := x86_64-unknown-linux-musl
IMAGE ?=
OCI ?=

dev:
	cargo build

check:
	cargo fmt --all
	cargo clippy --all-targets --all-features -- -D warnings || cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings

test:
	cargo nextest run

setup: dev
	target/debug/bunkerbox setup

musl-vscomm:
	cargo build --bin bunkerbox-vscomm --target $(VSCOMM_TARGET)

image: dev musl-vscomm
	@if [ -z "$(IMAGE)" ]; then echo "usage: make image IMAGE=images/name.conf" >&2; exit 1; fi
	target/debug/bunkerbox-image $(IMAGE)

install-image: dev
	@if [ -z "$(OCI)" ]; then echo "usage: make install-image OCI=path/to/image.oci" >&2; exit 1; fi
	BUNKERBOX_OCI_ARCHIVE=$(OCI) target/debug/bunkerbox install-image

prepare: dev
config: dev
	target/debug/bunkerbox config

	target/debug/bunkerbox prepare

$(DOCS_MKDOCS): docs/requirements.txt
	python3 -m venv $(DOCS_VENV)
	$(DOCS_VENV)/bin/pip install -r docs/requirements.txt

docs: $(DOCS_MKDOCS)
	$(DOCS_MKDOCS) build --strict

docs-clean:
	rm -rf target/site-docs $(DOCS_VENV)
