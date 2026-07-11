.PHONY: dev check setup image install-image prepare docs docs-clean

DOCS_VENV := .venv-docs
DOCS_MKDOCS := $(DOCS_VENV)/bin/mkdocs
IMAGE ?=
OCI ?=

dev:
	cargo build

check:
	cargo fmt --all
	cargo clippy --all-targets --all-features -- -D warnings || cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings

setup: dev
	target/debug/bunkerbox setup

image: dev
	@if [ -z "$(IMAGE)" ]; then echo "usage: make image IMAGE=images/name.conf" >&2; exit 1; fi
	target/debug/bunkerbox-image $(IMAGE)

install-image: dev
	@if [ -z "$(OCI)" ]; then echo "usage: make install-image OCI=path/to/image.oci" >&2; exit 1; fi
	BUNKERBOX_OCI_ARCHIVE=$(OCI) target/debug/bunkerbox install-image

prepare: dev
	target/debug/bunkerbox prepare

$(DOCS_MKDOCS): docs/requirements.txt
	python3 -m venv $(DOCS_VENV)
	$(DOCS_VENV)/bin/pip install -r docs/requirements.txt

docs: $(DOCS_MKDOCS)
	$(DOCS_MKDOCS) build --strict

docs-clean:
	rm -rf target/site-docs $(DOCS_VENV)
