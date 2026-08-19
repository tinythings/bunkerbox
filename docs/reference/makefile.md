# Makefile reference

The Makefile is the public development interface for this repository. Use it instead of calling internal tools directly.

## Build

Use this when you want to compile the project for local development:

```sh
make dev
```

The development binaries are written under `target/debug/`.

## Check

Use this before committing code:

```sh
make check
```

It formats the code and runs lint checks.

## mxrun Builds

Development, release, and testing targets use the local mxrun target when mxrun
is enabled:

```sh
make mxrun-init
make dev
make release
make test
```

The integration test target is delegated in the same way:

```sh
make integration-test
```

Use these targets to control delegation:

```sh
make mxrun
make set-local-builds
make set-remote-builds
```

Extra mxrun command-line options can be passed with `MXRUN_ARGS`:

```sh
MXRUN_ARGS="--mirror-results" make test
```

## Setup

Use this to prepare the host runtime pieces needed by Bunkerbox:

```sh
make setup
```

This is needed before local container runs can work correctly.

## Build an image

Use this to build an OCI archive from any image config:

```sh
make image IMAGE=images/name.conf
```

`IMAGE` is required. It points to the image config file. For example:

```sh
make image IMAGE=images/opencode.conf
```

The image config decides the output file name.

## Import an image

Use this to import an OCI archive into containerd for local development:

```sh
make install-image OCI=path/to/image.oci
```

`OCI` is required. For example:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

## Prepare workspace

Use this when you need the disposable workspace used by clone mode:

```sh
make prepare
```

The workspace is created under `.bunker/`.

## Build documentation

Use this to build the static documentation website:

```sh
make docs
```

The output is written to:

```text
target/site-docs/
```

## Clean documentation output

Use this to remove the generated documentation website and local documentation environment:

```sh
make docs-clean
```
