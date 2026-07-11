# Bunkerbox

Bunkerbox runs developer tools inside isolated Kata containers.

Use the Makefile for project tasks.

## Main jobs

- build project binaries
- build OCI images from image configs
- import OCI images for local development
- run tools with isolated workspace, home, and network settings
- build static documentation website
- package tools as normal commands

## Common commands

| Command | Use |
|---|---|
| `make dev` | Build project binaries |
| `make check` | Check code |
| `make image IMAGE=images/opencode.conf` | Build an OCI image from config |
| `make install-image OCI=bunkerbox-opencode-1.17.18.oci` | Import an OCI image |
| `make docs` | Build static documentation website |

## Read next

- [Quickstart](quickstart.md)
- [Concepts](concepts.md)
- [Packaging](guides/packaging.md)
- [Makefile reference](reference/makefile.md)
