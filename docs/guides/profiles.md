# Sandbox profiles

A profile is a small YAML file that tells Bunkerbox what a particular build
toolchain is allowed to do on your host. When profiles are active, every
passthrough command runs inside a bubblewrap sandbox built from those rules.

## Why profiles exist

Passthrough proxies commands from the VM to your host — but "proxied" does not
mean "safe". Without a sandbox, `cargo build` runs as *you*, with access to
your filesystem, your home directory, your network, and your environment
variables. An AI agent that can run arbitrary cargo commands can also run
arbitrary arbitrary code through `build.rs` or proc macros.

Profiles draw the line between "the agent can build the project" and "the
agent can do anything". They declare exactly which binaries can run, which
parts of your filesystem are visible, whether the command can talk to the
network, and which environment variables it inherits.

## Built-in profiles

Bunkerbox ships five profiles that cover the most common build systems. You
can use them by name — no files to write, no paths to manage.

**`rust`** — for projects with a `Cargo.toml`. Provides `cargo`, `rustc`,
`rustfmt`, and `cc`. Carries over `.cargo` and `.rustup` into the sandbox home
and sets `CARGO_HOME` and `RUSTUP_HOME` to their guest paths.

**`make`** — for projects with a `Makefile`. Provides `make`, `gcc`, `g++`,
`ar`, `ld`, `as`, and `strip`. No writable directories by default — make
output goes to the overlay workspace.

**`go`** — for projects with a `go.mod`. Provides `go` and `gofmt`. Mounts
the Go toolchain directory and carries over `go` and `.cache/go-build`. Sets
`GOROOT`, `GOPATH`, and `GOCACHE` to guest paths.

**`node`** — for projects with a `package.json`. Provides `node`, `npm`, and
`npx`. Carries over `.npm` and `.node-gyp` so packages are cached between
runs.

**`python`** — for projects with `pyproject.toml` or `setup.py`. Provides
`python3` and `pip3` (also aliased as `python` and `pip`). Carries over the
`.cache/pip` cache.

All profiles share the same base rules: system libraries (`/lib`, `/lib64`,
`/usr/lib`) are available at their standard paths, the network is disabled,
and the shell is `/bin/sh`.

## Using profiles

Add a `profiles` key to your `.bunkerbox/project.conf`:

```yaml
profiles:
  - rust
  - make
```

You can run the config wizard (`bunkerbox config`) to select profiles
interactively — it detects your build system and suggests the matching
profiles.

When multiple profiles are listed, their rules merge. If your project uses
both Cargo and Make, add both `rust` and `make`. The sandboxed command will
have access to the union of all binaries and directories from both profiles.

If `profiles` is empty or absent, passthrough commands run directly on the
host with no sandbox.

## Custom profiles

When a built-in profile does not cover your toolchain — or you want tighter
rules — write your own. A custom profile is a YAML file with the same
structure as the built-ins:

```yaml
name: my-toolchain

bin:
  my-compiler: /opt/toolchain/bin/my-compiler
  my-linker: /opt/toolchain/bin/my-linker

paths:
  - src: /lib
  - src: /lib64
  - src: /usr/lib
  - src: /opt/toolchain/lib
  - src: .cache/my-toolchain

env:
  TOOLCHAIN_HOME: /opt/toolchain
  TERM: "${TERM}"

network: none
shell: /bin/sh
```

Reference it by absolute path in your project config:

```yaml
profiles:
  - /home/bo/.config/bunkerbox/profiles/my-toolchain.yaml
```

### Fields

**`name`** — a human-readable label. Used for logging and debugging.

**`bin`** — the host executables the sandboxed command may access. Each
entry maps a command name to its absolute path on the host. The daemon
resolves the path against your `PATH` before mounting, so you can write
`cargo: /usr/bin/cargo` and it will still work if cargo lives at
`~/.cargo/bin/cargo` — the daemon finds it for you.

**`paths`** — host paths made available inside the sandbox. A relative `src`
is resolved below the host user's home and appears below `/home` in the guest:

```yaml
paths:
  - src: .cargo
  - src: /usr/lib/some/cpp/includes/crap
  - src: /opt/sdk/include
    dst: /toolchain/include
```

Relative paths are writable carryover data and use `/home/<src>` as the guest
destination. Absolute paths below the host home use the corresponding `/home`
destination. Absolute paths outside the host home keep the same destination and
are mounted as system/toolchain inputs. An explicit `dst` overrides the
destination. The profile author is responsible for the host paths selected.

**`env`** — guest environment variables set inside the sandbox. Use guest paths
such as `/home/.cargo` for toolchain configuration (`CARGO_HOME`, `GOPATH`,
etc.). `${HOME}` expands to `/home`; `${USER}` and `${TERM}` use the host
runtime values when present.

**`network`** — currently only `none` is supported. The sandboxed command has
no network access.

**`shell`** — the absolute path to the shell used when the command specifies
`/bin/sh` as its interpreter. Defaults to `/bin/sh`.

## How rules translate to isolation

When a profile is active, each binary in the list is bind-mounted at its
expected path inside the sandbox. If the binary is a symlink (common
with rustup, where `cargo` and `rustc` both point to the same `rustup`
binary), the daemon follows the link and mounts the real file — so the
sandbox sees a working executable, not a dangling symlink.

Absolute system and toolchain paths are mounted at their standard destinations.
Home-relative paths are plain writable carryover binds, so changes are visible
on the host. The overlay workspace at `.bunkerbox/workspace` is always mounted
read-write at `/workspace` inside the sandbox, so build output always lands in
the copy-on-write layer.

The command gets a clean environment: no host variables leak in, and the
profile's `env` block provides exactly what the toolchain needs. `/proc` and
`/dev` are the sandbox's own — the command cannot see host processes or raw
devices. `/tmp` and `/home` are empty tmpfs mounts, discarded when the
command exits.

Bubblewrap provides the namespace boundary. Host paths selected by a profile
remain trusted profile policy; path declarations are not a substitute for
capability dropping or a non-root threat model.
