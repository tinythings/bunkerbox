# Config schema

This page shows the current YAML shapes used by Bunkerbox.

## Image config

An image config lives under `images/`. It describes how to build one OCI archive.

```yaml
name: string
image: string
output: path
overwrite: bool
command:
  - string
build_args:
  NAME: value
hooks:
  before-home-load: shell
  before-app: shell
  after-app: shell
  app-error: shell
  after-home-save: shell
files:
  - path: relative/path
    mode: "0755"
    content: string
containerfile: string
```

Build an image config with:

```sh
make image IMAGE=images/opencode.conf
```

## Runtime config

A runtime config describes how a packaged command runs an image.

```yaml
oci: path
image: string
workspace: share | clone | cow | direct | isolated
workspace_quota: string     # 10G, 500M, etc. (fallback default)
workspace_exclude:          # fallback exclude pattern list
  - target/
home: persist | temporary
home_path: path
network: bridge | host
allow:
  - hostname
```

## Workspace env.conf

When using copy-on-write mode, per-project settings are stored in `.bunkerbox/env.conf` (auto-generated on first run). This file takes precedence over the runtime config.

```yaml
# Bunkerbox workspace configuration
# Edit this file to customize behavior.

# Quota for copy-on-write workspace. "auto" = walk repo (skipping excluded dirs), +10%, floor 1G.
# Use "10G", "500M", etc. for an explicit size.
quota: auto

# Directories excluded from copy-on-write (stored on host disk instead of in the capped loopback).
# Patterns match directory names relative to the repository root.
exclude:
  - target/
  - node_modules/
  - .venv/
  - venv/
  - build/
  - __pycache__/
  - dist/
  - .next/
  - .gradle/
  - cmake-build-debug/
  - cmake-build-release/
```

During development, runtime configs live in `runtime/`. In a packaged install, they live under:

```text
/usr/share/bunkerbox/
```

For a command named `opencode`, the packaged runtime config is:

```text
/usr/share/bunkerbox/opencode.conf
```

## Modes

`workspace` decides how the project is mounted. Use `cow` (or the old alias `share`) for copy-on-write with a capped loopback, `direct` for direct mounting, and `isolated` (or the old alias `clone`) for a disposable workspace.

`home` decides whether app state is saved. Use `persist` to save state and `temporary` to throw it away after the run.

`network` decides how the container gets network access. Use `bridge` for isolated bridge networking and `host` for host networking.
