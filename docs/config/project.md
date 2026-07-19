# Project config

A project config tunes Bunkerbox behaviour for a specific repository. It lives
at `.bunkerbox/project.conf` and is auto-generated on first run.

You can create and edit this file interactively:

```sh
bunkerbox config
```

The wizard detects your build system, asks you a few questions, and writes a
clean `project.conf` — no YAML knowledge needed. Pass `--share <dir>` to see
current values from a packaged runtime config.

The runtime config answers "how should this tool run on *any* machine?" The
project config answers "how should this tool run on *this* project?" The
runtime config ships in the package and is immutable; the project config lives
in your repo and you edit it freely.

## Example

```yaml
# Bunkerbox project configuration
# Edit this file to customize behavior.

project:
  quota: auto
  exclude:
    - vendor/
    - docs/
  passthrough:
    - "cargo *"
    - "make *"
    - "go *"

# Override shared runtime defaults (uncomment to use):
# image:
#   workspace: direct
#   session_mb: 200
#   allow:
#     - extra.api.example.com
```

## Sections

### `project`

Settings that describe *this* repository.

**`env`** — controls how the guest VM environment is handled when proxying
commands to the host. Two modes:

- `relaxed` (default) — guest environment variables pass through to the host
  command, except `HOME`, `PATH`, `XDG_*`, and `BUNKERBOX_*` which are
  stripped.
- `paranoid` — the guest environment is fully dropped. Commands inherit the
  host daemon's environment only. Glob patterns in `passthrough` are
  **rejected** — every entry must be an exact command name.

Use `paranoid` when you want zero guest influence over host command execution.
The AI can't set `LD_PRELOAD`, `RUSTFLAGS`, `PYTHONPATH`, or any other
variable to manipulate the host toolchain.

**`quota`** — upper-layer size limit for the copy-on-write workspace. Accepts
`auto` (walk the repo, skip excluded dirs, add 10%, floor 5 GB), an explicit
size string (`"20G"`, `"500M"`, `"1048576"`), or omit for auto. This controls
how large the `upper.img` loopback file can grow.

**`exclude`** — list of directory patterns to skip during the auto-quota walk.
These directories still live inside the loopback image (their writes count
against the quota). They are merely excluded from the size estimate, not from
the overlay. Common additions: `vendor/`, `docs/`, `out/`.

**`passthrough`** — list of commands the AI agent is allowed to proxy from the
VM to your host machine via vsock. Each entry is either an exact command name
(`"make"` matches `make` with zero arguments only) or a command with an
argument glob (`"make *"` matches `make` with any arguments, including zero).
See the [Passthrough guide](../guides/passthrough.md) for the full
architecture.

If `passthrough` is empty when the config is first loaded, Bunkerbox scans the
repository for known build system files (`Cargo.toml`, `Makefile`,
`package.json`, `go.mod`, `CMakeLists.txt`, `pyproject.toml`, `pom.xml`,
`build.gradle`, `meson.build`) and pre-fills the list automatically.

### `image`

Overrides that change how the shared runtime config applies to *this* project.
The `image` section is optional — leave it absent (or fully commented out) and
the runtime defaults take effect.

**`workspace`** — override the workspace mode: `cow` (default, overlayfs with
quota), `direct` (mount repo directly, no guardrails), or `isolated` (clone
the repo via `git worktree`).

**`session_mb`** — override the session image size in megabytes. Default 50.
Set 0 to disable the loop-mounted session image and bind-mount the raw home
directory directly.

**`allow`** — extra network destinations appended to the runtime config's
allow list. The base `allow` list from the runtime config is **never removed** —
the packager sets the minimum. You can only add destinations, not subtract
them.

Fields that **cannot** be overridden in `image:`:

- `network` — bridge/host mode is set by the runtime config only
- `oci` — the OCI archive path is determined at package install time
- `encrypt` — encryption patterns are security-sensitive, shared config only

## Auto-detection

When `passthrough` is empty on config load, Bunkerbox scans the repo root for
build system marker files and populates the list automatically. Nine detectors
are built in:

| Detects | Adds |
|---|---|
| `Cargo.toml` | `cargo *` |
| `Makefile` / `makefile` / `GNUmakefile` | `make *` |
| `package.json` | `npm *`, `npx *` |
| `pyproject.toml` / `setup.py` / `setup.cfg` | `python *`, `pip *` |
| `go.mod` | `go *` |
| `CMakeLists.txt` | `cmake *`, `make *` |
| `build.gradle*` / `settings.gradle*` / `gradlew` | `gradle *`, `./gradlew *` |
| `pom.xml` | `mvn *` |
| `meson.build` | `meson *`, `ninja *` |

If your project has both a `Cargo.toml` and a `Makefile`, both show up.
Detection only runs when the list is empty — once you've added a command,
you're in full control.

## Legacy migration

Older versions of Bunkerbox used `.bunkerbox/env.conf` with a flat structure.
When the new `.bunkerbox/project.conf` is not found, Bunkerbox checks for the
old `env.conf`, migrates its fields into the `project:` section, writes
`project.conf`, and deletes the old file. This is automatic and silent. No
user action required.

## How the VM reads it

Inside the container, the project directory is mounted at `/workspace`.
The file `/workspace/.bunkerbox/project.conf` is the same file you see on the
host — it's the bind-mounted overlay workspace. When the container boots, the
`bunkerbox-vscomm install` step reads the `passthrough` list and creates
symlinks for each whitelisted command that is **not** already present in the
VM. The symlinks live at `/usr/local/bunkerbox/bin/` which is prepended to
`PATH`.
