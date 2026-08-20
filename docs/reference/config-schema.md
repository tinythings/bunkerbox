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
session_mb: int             # session image size in MB, default 50, 0 to disable (host-side loop mount)
network: bridge | host
allow:
  - hostname
```

## Workspace project.conf

When using copy-on-write mode, per-project settings are stored in `.bunkerbox/project.conf` (auto-generated on first run). This file takes precedence over the runtime config.

For full documentation of every field, see [Project config](../config/project.md).

```yaml
# Bunkerbox project configuration
# Edit this file to customize behavior.

project:
  # Quota for copy-on-write workspace. "auto" = walk repo (skipping excluded dirs), +10%, floor 5G.
  # Use "10G", "500M", etc. for an explicit size.
  quota: auto

  # Directories excluded from the auto-quota walk (their output still uses the loopback image).
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

  # Passthrough: commands proxied from VM to host via vsock.
  # "make *" matches with any args. "make" matches only exact (no args).
  # Auto-detected on first run if empty.
  passthrough:
    - "make *"
    - "cargo *"

  # Optional host-owned transparent remote policy.
  remote:
    exclude: [target/]
    environment: [CC, CXX]
    tools:
      - name: make
        allow-args: true
      - name: cargo
        allow-args: true
    artifacts: [build/app]

# Override shared runtime defaults (optional, uncomment to use):
# image:
#   workspace: direct
#   home: persist
#   home_path: /custom/path
#   session_mb: 200
#   allow:
#     - extra.api.example.com
```

## Project-local remote.conf

Remote targets are optional and live at `.bunkerbox/remote.conf`. The file
contains only compact target definitions. SSH aliases, identities, host-key
state, and credentials remain in the host OpenSSH configuration.

```yaml
targets:
  netbsd:
    ssh: builder@netbsd-builder:2222
    workspace: /var/tmp/bunkerbox
    project:
      remote:
        tools:
          - name: make
            command: gmake
            allow-args: true
    resources:
      build-timeout-seconds: 3600
      max-active-builds: 1
```

`ssh` accepts `[user@]host[:port]` or a host-side OpenSSH alias. `workspace`
must be an absolute normalized target path. `localhost` is implicit, always
listed first, and selected at startup. Press `Ctrl+Alt+B` in the host TUI to
choose another target; `Ctrl+Alt+S` opens Remote Setup; and `Ctrl+Alt+H` shows
the host controls. Remote Setup edits only the compact target fields and the
`project.remote` overlay: tool allowlists, environment names, snapshot
exclusions, artifact paths, and optional resource limits. Blank resource fields
remain unset, and omitted overlay fields continue inheriting `project.conf`.
Save validates the complete draft and atomically writes the file for the next
Bunkerbox run; it does not change the current catalog, target, or backend and
does not perform SSH probing. Existing malformed or unsafe configurations are
reported rather than replaced. Target selection is frozen per transaction, and
remote failures do not retry locally.

## Sandbox profile

Profiles are host-side YAML files selected by the project configuration.

```yaml
name: rust
bin:
  cargo: /usr/bin/cargo
paths:
  - src: /lib
  - src: /usr/lib
  - src: .cargo
  - src: .rustup
  - src: /opt/sdk/include
    dst: /toolchain/include
env:
  CARGO_HOME: /home/.cargo
  RUSTUP_HOME: /home/.rustup
network: none
shell: /bin/sh
```

Relative `src` paths are resolved below the host user's home and appear below
`/home` in the guest. Absolute paths below the host home use the corresponding
`/home` destination. Absolute paths outside the host home retain their source
path as the guest destination unless `dst` is supplied. Home-relative paths are
writable carryover data; absolute system and toolchain paths are read-only
inputs by default. These declarations are trusted host policy.

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

`home` decides whether app state is saved. Use `persist` to save state and `temporary` to throw it away after the run. When persistence is enabled, `session_mb` sets the host-side loop-mounted ext4 image size in MB (default 50, set 0 to bind-mount the raw persist home directly).

`network` decides how the container gets network access. Use `bridge` for isolated bridge networking and `host` for host networking.
