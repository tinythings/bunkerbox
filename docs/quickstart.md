# Quickstart

This page shows the normal development flow. Use the Makefile for project tasks.

## Build the project

Start by building the project binaries.

```sh
make dev
```

This creates the local development binaries under `target/debug/`.

## Check the project

Before committing changes, run the project checks.

```sh
make check
```

This formats the code and runs lint checks.

## Setup the host runtime

Bunkerbox uses host runtime pieces such as containerd and Kata. Prepare them with:

```sh
make setup
```

This is a host setup step. It should be done before trying to run isolated tools locally.

## Build an image

Images are built from configs in `images/`. For example:

```sh
make image IMAGE=images/opencode.conf
```

The `IMAGE` value is the path to the image config. The config itself decides the output archive name. For the OpenCode config, the output is:

```text
bunkerbox-opencode-1.17.18.oci
```

## Import an image

For local development, import the OCI archive into containerd.

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

The `OCI` value is the archive that should be imported.

## Prepare a workspace

If you use clone workspace mode, prepare the disposable workspace with:

```sh
make prepare
```

The prepared workspace lives under `.bunker/` and is ignored by the repository.

## Build the docs site

The documentation is a static website.

```sh
make docs
```

The generated site is written to:

```text
target/site-docs/
```
