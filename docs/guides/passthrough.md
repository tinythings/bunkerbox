# Passthrough

Running host build tools from inside the isolated VM.

## The problem

The container image ships a minimal Alpine Linux. You want the AI agent to run
`cargo test`, `make build`, `go vet`, or `npm install` against your project. None
of these tools are inside the VM — and installing them in the container defeats
the purpose of isolation. Besides, your project might depend on Fedora toolchains,
musl-compatible binaries, or system libraries that simply do not exist inside the
VM.

Passthrough solves this. It gives the AI agent transparent access to your host's
build tooling, without letting the agent touch anything outside the workspace.

## How it works

When the container boots, a small static binary called `bunkerbox-vscomm` reads
the whitelist from your project's `.bunkerbox/env.conf`. For each whitelisted
command that is **not** already present inside the VM, it creates a symlink in
`/usr/local/bunkerbox/bin/` and prepends that directory to `PATH`.

When the AI agent invokes one of those commands, the symlink points to
`bunkerbox-vscomm`, which proxies the call through a virtio-vsock channel to a
daemon running on the host. The daemon checks the whitelist one more time,
spawns the real command inside the overlay workspace at `.bunkerbox/workspace/`,
and streams stdout, stderr, and the exit code back.

```
┌─ Kata VM ──────────────────────────────────────────┐
│  AI agent calls "make build"                       │
│    │                                               │
│    ├─ /usr/local/bunkerbox/bin/make                │
│    │       │                                       │
│    │       └─ symlink → bunkerbox-vscomm           │
│    │              │                                │
│    │              │  vsock (port 9999)             │
│    │              ▼                                │
│    │       "run make build in /workspace"          │
└────┼───────────────────────────────────────────────┘
     │  virtio-vsock
┌────┼───────────────────────────────────────────────┐
│    ▼                                               │
│  Host daemon (tokio, embedded in bunkerbox binary) │
│    │                                               │
│    ├─ whitelist check: "make *" ✓                  │
│    ├─ cd .bunkerbox/workspace/                     │
│    ├─ spawn make build                             │
│    ├─ stream stdout / stderr back                  │
│    └─ send exit code                               │
└────────────────────────────────────────────────────┘
```

The AI agent sees standard output exactly as if `make` ran locally. The host
daemon runs inside the overlay workspace, so all output — compiled binaries,
generated files, test results — lands in the upper layer of the overlay and is
auto-synced back to your real repo when the container exits.

## Configuration

The whitelist lives in `.bunkerbox/env.conf` under the `passthrough` key:

```yaml
# Passthrough commands proxied from VM to host via vsock.
passthrough:
  - "make *"     # make with any arguments
  - "cargo *"    # cargo build, cargo test, cargo clippy...
  - "go *"       # go build, go test, go vet...
```

Two syntax forms are supported:

| Entry | Matches |
|---|---|
| `"make"` | `make` with **no** arguments only |
| `"make *"` | `make` with **any** arguments (including zero) |

The trailing `*` is a glob on arguments, not a shell wildcard. `"make *"` means
"allow `make` with any number of arguments." `"make"` means "allow `make` only
when called with no arguments at all."

Commands that are not in the whitelist are not proxied. If the VM also lacks
them, the agent gets a "command not found" — as it should be inside a bunker.

## Auto-detection

When `.bunkerbox/env.conf` is first created (or if the `passthrough` list is
empty on startup), Bunkerbox scans the repository root for known build system
files and pre-fills the whitelist automatically:

| Build system | Detected by |
|---|---|
| GNU Make | `Makefile`, `makefile`, `GNUmakefile` |
| Cargo | `Cargo.toml` |
| npm | `package.json` |
| Python | `pyproject.toml`, `setup.py`, `setup.cfg` |
| Go | `go.mod` |
| CMake | `CMakeLists.txt` |
| Gradle | `build.gradle`, `build.gradle.kts`, `settings.gradle`, `gradlew` |
| Maven | `pom.xml` |
| Meson | `meson.build` |

If your project has both a `Makefile` and a `Cargo.toml`, both `make *` and
`cargo *` are added. The auto-detection only runs once — afterwards, you edit
the list yourself. If you delete all entries and leave it empty, detection runs
again on the next start.

## VM always wins

Passthrough only fills gaps. If the command already exists inside the VM — for
example, you baked `gcc` or `python` into a custom container image — no symlink
is created for it. The VM's native version runs. This rule is checked at every
boot, so removing a tool from the image is sufficient to let passthrough take
over.

## Security model

**No unsolicited access.** Only commands explicitly listed in `passthrough` get
proxied. The agent cannot call arbitrary host binaries.

**No host exposure.** The daemon runs every command inside
`.bunkerbox/workspace/` — the overlay mount point, not your real repository. The
agent's work is isolated in the copy-on-write layer.

**No system commands.** The whitelist is designed for build tools (`make`,
`cargo`, `go`, `npm`, `meson`, etc.). There is no reason to put `rm`, `dd`,
`curl`, `ssh`, or `sudo` in the whitelist — and if you do, you take the risk.

**Environment sanitization.** The guest VM passes its environment to the host
daemon. Before spawning, the daemon strips `HOME`, `XDG_*`, `PATH`, `VSOCK_CID`,
and `BUNKERBOX_*` variables from the guest environment. The command inherits the
host's real `HOME`, so caches like `~/.cargo/registry`, `~/.cache/go-build`, and
`~/.npm` are shared across sessions — no repeated downloads.

**Quota.** The overlay workspace has a capped loopback image (`upper.img`). Its
size is controlled by the `quota` setting in `env.conf`. The default auto-quota
is 5 GB. Set `quota: 20G` or higher if your builds produce large artifacts.

## Requirements

The host needs the `vhost_vsock` kernel module. Most distributions include it by
default. Verify with:

```sh
lsmod | grep vsock
```

If missing:

```sh
sudo modprobe vhost_vsock
```

The module must be loaded before starting Bunkerbox. If vsock is unavailable, the
daemon prints a warning at startup and the container runs without passthrough
support — build commands that aren't in the VM will simply be unavailable.
