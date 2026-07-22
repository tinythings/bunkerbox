<img width="1536" height="1024" alt="bunkerbox" src="https://github.com/user-attachments/assets/563f83ce-f93c-45da-94a1-a5aa29c1e952" />


# Bunkerbox

[![CI](https://github.com/tinythings/bunkerbox/actions/workflows/ci.yml/badge.svg)](https://github.com/tinythings/bunkerbox/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.97.1-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/github/license/tinythings/bunkerbox)](https://github.com/tinythings/bunkerbox/blob/main/LICENSE)
[![Release](https://img.shields.io/github/v/release/tinythings/bunkerbox)](https://github.com/tinythings/bunkerbox/releases)
[![Docs](https://github.com/tinythings/bunkerbox/actions/workflows/docs.yml/badge.svg)](https://github.com/tinythings/bunkerbox/actions/workflows/docs.yml)

**Run powerful development agents without handing them your whole machine.**

Bunkerbox is an isolation layer for coding agents, CLIs, and other developer tools that need to work inside a project directory. Instead of running those tools directly on the host, Bunkerbox runs them inside a Kata-backed container with a controlled workspace, controlled home directory, and explicit runtime configuration.

It is built for the world where developer tools are becoming more capable, more autonomous, and more deeply integrated into local workflows. Those tools are useful, but they should not automatically inherit your full shell environment, your host home directory, your credentials, and your entire filesystem. Bunkerbox gives them a smaller box to work in.

## What Bunkerbox gives you

A tool launched through Bunkerbox sees the project workspace it needs, but not the whole host. Its application state can be persisted between runs without exposing the real user home. Its image is built ahead of time from a reproducible config. Its runtime behavior is described separately, so packaging a tool is a matter of pairing an OCI image with a small runtime config.

When the agent runs a build command like `cargo build`, that command executes on your host — but not freely. Bunkerbox wraps it in a bubblewrap sandbox that strips the environment, blocks the network, and exposes only the tools and directories declared in a sandbox profile. The agent can compile your code. It cannot read your SSH keys, curl a payload, or peek at host processes.

The result is a workflow where tools still feel like normal commands, but run with a stronger boundary around them.

## How it feels

A packaged tool can be invoked like any other command:

```sh
opencode
```

Behind that command, Bunkerbox loads the matching runtime configuration, imports the prepared OCI image when needed, mounts the project workspace, prepares the tool home, applies network settings, and starts the tool inside a Kata container.

For development, the repository exposes the workflow through Makefile targets. Build an image from an image config:

```sh
make image IMAGE=images/opencode.conf
```

Import an OCI archive for local development:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

Build the documentation website:

```sh
make docs
```

The generated documentation site is written to `target/site-docs/`.

## Images, runtimes, and packaging

Bunkerbox separates image building from runtime behavior. Image configs live in `images/` and describe how a tool image is built. Runtime configs live in `runtime/` and describe how that image should be executed: workspace mode, home persistence, network mode, and allowed destinations.

In a packaged install, a command such as `opencode` can be a symlink to the Bunkerbox binary. Bunkerbox uses the invoked command name to load the matching config from `/usr/share/bunkerbox`. That makes packaged tools feel ordinary while keeping the isolation behavior centralized.

## Status

Bunkerbox is a proof of concept. It is focused on exploring a safer local execution model for agentic developer tools using OCI images, containerd, and Kata Containers.

## Documentation

The full documentation is built as a static website:

```sh
make docs
```

Open the generated site from:

```text
target/site-docs/
```

The project also includes Read the Docs configuration for public documentation hosting.
