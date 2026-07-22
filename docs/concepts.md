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

## Sandbox

Passthrough gets the command to your host — but what stops it from running wild
once it's there? That's the sandbox.

When profiles are configured in `project.conf`, the host daemon does not spawn
passthrough commands directly. It wraps each one inside a
[bubblewrap](https://github.com/containers/bubblewrap) sandbox. Bubblewrap uses
Linux user namespaces to create a thin, unprivileged container around the
command with exactly the capabilities it needs and nothing more.

A sandbox profile is a small YAML file that declares:

- Which exact host binaries the command may access (bind-mounted read-only)
- Which directories are visible and whether they are writable
- Whether network access is permitted
- Which environment variables the command inherits

Bunkerbox ships profiles for common build systems — `rust`, `make`, `go`,
`node`, `python` — and you can write your own. When multiple profiles are
active, they merge: the union of all binaries and directories is available to
the sandboxed command.

Inside the sandbox, the command sees a scratch `/home`, an empty `/tmp`, its
own `/proc`, no network, and only the binaries you explicitly allowed. It
cannot read your SSH keys, curl a payload, enumerate host processes, or write
anywhere outside the overlay workspace.

See the [Profiles guide](guides/profiles.md) for the full reference.

## Packaging

Packaging makes a tool feel like a normal command. The package installs the Bunkerbox binary, an app command symlink, a runtime config, and the OCI archive.

When the user runs the app command, Bunkerbox uses that command name to find the matching runtime config.
