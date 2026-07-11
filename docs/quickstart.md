# Quickstart

Use only Makefile commands.

## Build project

```sh
make dev
```

This builds the project binaries into `target/debug/`.

## Check project

```sh
make check
```

This formats and lints the project.

## Setup host runtime

```sh
make setup
```

This prepares host runtime pieces needed for containerd and Kata.

## Build an OCI image

```sh
make image IMAGE=images/opencode.conf
```

`IMAGE` is the image config path.

The config decides the output file.
For `images/opencode.conf`, output is:

```text
bunkerbox-opencode-1.17.18.oci
```

## Import an OCI image

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

`OCI` is the OCI archive path.

## Prepare workspace

```sh
make prepare
```

This creates a disposable workspace when clone workspace mode is used.

## Build docs website

```sh
make docs
```

This creates the static website in:

```text
target/site-docs/
```
