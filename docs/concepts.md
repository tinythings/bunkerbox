# Concepts

Bunkerbox has a few moving parts. Each part has a simple job.

## Image config

An image config describes how to build the tool image. Image configs live in `images/`.

For example, `images/opencode.conf` describes how to build an OCI archive that contains OpenCode and the generated Bunkerbox entrypoint.

Build an image config with:

```sh
make image IMAGE=images/opencode.conf
```

The image config decides the output archive name. The Makefile command only tells Bunkerbox which config to use.

## Runtime config

A runtime config describes how an image should run. Runtime configs live in `runtime/` during development.

A runtime config answers questions like: Which OCI archive should be used? Should the project be mounted directly or cloned first? Should app state be saved? Which network mode should be used?

In a packaged install, runtime configs are placed under:

```text
/usr/share/bunkerbox/
```

If the command is named `opencode`, Bunkerbox looks for:

```text
/usr/share/bunkerbox/opencode.conf
```

## Workspace

The workspace is the project directory that the tool can work on. Inside the container, the workspace is mounted at:

```text
/workspace
```

In `share` mode, Bunkerbox mounts the current project directly. In `clone` mode, Bunkerbox prepares a disposable copy under `.bunker/workspace`.

## App home

Many tools save config, cache, history, sessions, or login state in their home directory. Bunkerbox does not use your real home directory for this. Instead, it prepares a separate home for the tool.

When persistence is enabled, that home is saved between runs. By default, Bunkerbox creates a loop-mounted ext4 image file (`session.img`) on the host before starting the VM. The loop mount is bind-mounted into the VM, so the app writes through the ext4 journal. If the VM crashes, the image survives on the host disk and is recovered automatically on the next run.

Setting `session_mb: 0` disables the loop mount. The raw persist home directory is bind-mounted directly into the VM.

## Hooks

Hooks are shell snippets inside an image config. They run inside the container around important lifecycle moments, such as before the app starts or after it exits.

Hooks are useful for small setup and cleanup tasks. For example, a hook can prepare config files, mark `/workspace` as a safe Git directory, or remove cache before state is saved.

## Passthrough

Many projects need build tools like `cargo`, `make`, or `go` that are not
installed inside the isolated VM. Passthrough proxies whitelisted commands from
the VM to the host via vsock, so the AI agent can run builds, tests, and
linters transparently — without giving it real access to your host machine.

All commands run inside the overlay workspace. Output streams back to the agent
in real time. When the container exits, changes are auto-synced to your
repository.

See the [Passthrough guide](guides/passthrough.md) for setup and
configuration.

## Packaging

Packaging makes a tool feel like a normal command. The package installs the Bunkerbox binary, an app command symlink, a runtime config, and the OCI archive.

When the user runs the app command, Bunkerbox uses that command name to find the matching runtime config.
