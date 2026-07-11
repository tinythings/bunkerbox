# Makefile reference

Use `make` for all project tasks.

## Targets

| Target | What it does |
|---|---|
| `make dev` | Builds project binaries into `target/debug/` |
| `make check` | Formats and lints the project |
| `make setup` | Sets up host runtime pieces |
| `make image IMAGE=images/name.conf` | Builds an OCI archive from an image config |
| `make install-image OCI=path/to/image.oci` | Imports an OCI archive into containerd |
| `make prepare` | Prepares a disposable workspace |
| `make docs` | Builds the static documentation website into `target/site-docs/` |
| `make docs-clean` | Removes generated docs output and docs environment |

## Build binaries

```sh
make dev
```

Use this before other development targets.

## Check code

```sh
make check
```

This runs formatting and lint checks.

## Setup host runtime

```sh
make setup
```

This prepares host runtime dependencies used by Bunkerbox.

## Build image config

```sh
make image IMAGE=images/name.conf
```

`IMAGE` is required.

Example:

```sh
make image IMAGE=images/opencode.conf
```

The image config decides the output OCI archive path.

## Import OCI archive

```sh
make install-image OCI=path/to/image.oci
```

`OCI` is required.

Example:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

## Prepare workspace

```sh
make prepare
```

This creates the disposable workspace used by clone workspace mode.

## Build docs

```sh
make docs
```

This builds the static website into:

```text
target/site-docs/
```
