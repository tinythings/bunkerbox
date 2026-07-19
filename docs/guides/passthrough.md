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
the whitelist from your project's `.bunkerbox/project.conf`. For each whitelisted
command that is **not** already present inside the VM, it creates a symlink in
`/usr/local/bunkerbox/bin/` and prepends that directory to `PATH`.

When the AI agent invokes one of those commands, the symlink points to
`bunkerbox-vscomm`, which proxies the call through a virtio-vsock channel to a
daemon running on the host. The daemon checks the whitelist one more time,
spawns the real command inside the overlay workspace at `.bunkerbox/workspace/`,
and streams stdout, stderr, and the exit code back.

```
┌─ Bunkerbox VM ──────────────────────────────────────┐
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

The whitelist lives in `.bunkerbox/project.conf` under the `passthrough` key:

```yaml
# Passthrough commands proxied from VM to host via vsock.
passthrough:
  - "make *"     # make with any arguments
  - "cargo *"    # cargo build, cargo test, cargo clippy...
  - "go vet"     # go vet with any additional arguments
```

Three syntax forms are supported:

| Entry | Matches |
|---|---|
| `"make"` | `make` with **no** arguments only |
| `"make build"` | `make build` plus any additional arguments |
| `"make *"` | `make` with **any** arguments (including zero) |

The trailing `*` is a glob on arguments, not a shell wildcard. `"make *"` means
"allow `make` with any number of arguments." `"make"` means "allow `make` only
when called with no arguments at all." `"make build"` matches the default
make target plus arbitrary extra arguments.

Glob patterns are allowed only in `relaxed` and `dangerous` modes. `paranoid`
mode rejects them and requires exact commands or exact subcommands.

Commands that are not in the whitelist are not proxied. If the VM also lacks
them, the agent gets a "command not found" — as it should be inside a bunker.

## Auto-detection

When `.bunkerbox/project.conf` is first created (or if the `passthrough` list is
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
proxied.

**Filesystem sandbox.** In `relaxed` and `paranoid` modes, every proxied command
runs inside a bubblewrap sandbox. The sandbox can see only read-only system
directories (`/usr`, `/bin`, `/lib`, etc.) and the workspace mounted at
`/workspace`. It has no access to your real `$HOME`, `~/.ssh`, `~/.aws`, or other
projects. A malicious `Makefile` or `build.rs` can still execute, but it cannot
read or write outside the workspace.

**No network from passthrough.** By default the sandbox drops network access
entirely (`--unshare-all`). A malicious build script cannot phone home or
exfiltrate code over the network from the host.

**`dangerous` mode.** You can disable the sandbox with `env: dangerous`. In
that mode commands run directly on the host with the same environment filtering
as `relaxed`. This is an explicit opt-out and should be used only when you
really need an unsandboxed host tool.

**Environment sanitization.** In `relaxed` and `dangerous` modes, the daemon
strips `HOME`, `PATH`, `XDG_*`, `VSOCK_CID`, and `BUNKERBOX_*` from the guest
environment. In `paranoid` mode the guest environment is dropped entirely.

**Quota.** The overlay workspace has a capped loopback image (`upper.img`). Its
size is controlled by the `quota` setting in `project.conf`. The default auto-quota
is 5 GB. Set `quota: 20G` or higher if your builds produce large artifacts.

## Requirements

The host needs:

- The `vhost_vsock` kernel module (for the vsock passthrough channel).
- [bubblewrap](https://github.com/containers/bubblewrap) version **0.10.0 or newer**
  (unless you run with `env: dangerous`).

If `bwrap` is not installed or is too old, install it from your distribution
package manager or build it from source:

```sh
git clone https://github.com/containers/bubblewrap.git
cd bubblewrap
git checkout v0.11.0
meson setup build
meson compile -C build
# use build/bwrap in .bunkerbox/project.conf:
#   sandbox:
#     bwrap: /absolute/path/to/build/bwrap
```

### vhost_vsock

Most distributions include it by default. Verify with:

```sh
lsmod | grep vsock
```

If missing:

```sh
sudo modprobe vhost_vsock
```

The module must be loaded before starting Bunkerbox. If vsock is unavailable, the

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
