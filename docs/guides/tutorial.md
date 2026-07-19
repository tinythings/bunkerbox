# Tutorial

A step-by-step walkthrough — from zero to running KiloCode inside a Kata
bunker with passthrough builds.

## Prerequisites

An Ubuntu 22.04 or 24.04 machine with:

- Rust 1.97+ (`rustup` recommended)
- `make`, `curl`, `sudo`
- At least 8 GB free disk space

## Step 1 — Clone and build Bunkerbox

```sh
git clone https://github.com/bunker-project/bunkerbox
cd bunkerbox
```

## Step 2 — Run the demo setup

The demo script downloads Kata Containers, builds the KiloCode OCI image, and
creates a `kilo` launcher symlink. Nothing is installed system-wide.

```sh
./demo/kilocode/run-demo.sh
```

This takes a few minutes the first time (Kata download + Rust build). On
subsequent runs it finishes in seconds.

Two things to note during the run:

- **Passphrase prompt.** The first time, press Enter twice to accept an empty
  passphrase (no credential encryption). You can set one later.
- **Wait for "Demo ready."** The script prints confirmation when everything is
  set up.

## Step 3 — Verify the launcher

```sh
cd demo/kilocode
./kilo --version
```

You should see the Bunkerbox version. The `kilo` symlink points to the
`bunkerbox` binary — it auto-detects the invocation name and loads the matching
runtime config from `./share/kilo.conf`.

## Step 4 — Create a test project

In a fresh directory, create a tiny Rust project:

```sh
mkdir ~/bunkerbox-tutorial
cd ~/bunkerbox-tutorial
cargo init --name hello
```

Replace the auto-generated `src/main.rs` with something that compiles but has a
deliberate mistake so the AI has something to fix:

```rust
fn main() {
    let numbers = vec![1, 2, 3, 4, 5];
    let total: i32 = numbers.iter().sum();
    println!("sum is {total}")
}
```

This is a working program — but it's missing a `use` statement. The AI should
discover `sum()` needs `use std::iter::Sum;` or switch to `.into_iter()`.

## Step 5 — Prepare the Bunkerbox workspace

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share prepare
```

This creates the `.bunkerbox/` directory. On first run, Bunkerbox auto-detects
the build system (Rust / Cargo) and pre-fills `.bunkerbox/env.conf` with:

```yaml
quota: auto
exclude:
  - target/
  - ...
passthrough:
  - "cargo *"
```

Check that the passthrough section is populated:

```sh
cat .bunkerbox/env.conf | grep -A2 passthrough
```

## Step 6 — Invoke the AI

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share
```

KiloCode starts. Give it a prompt like:

> Please build this project with cargo build, then run cargo test, and fix
> any compilation errors.

The AI will call `cargo build`, `cargo test`, and `cargo clippy` as needed.
Each call goes through the vsock passthrough — the commands run on your host
machine, inside the overlay workspace. The AI sees the real build output and
fixes issues iteratively.

## Step 7 — Verify the results

After the container exits, Bunkerbox auto-syncs all overlay changes back to
your real project. Check your files:

```sh
ls -la src/
cat src/main.rs
git diff
```

All changes the AI made are now in your working tree. Review, stage, and commit
as usual.

## Step 8 — Repeat

Any time you want the AI to work on this project again:

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share
```

The workspace is persistent — the overlay upper layer survives between sessions
unless you `bunkerbox prepare --reset`. The `env.conf` is already configured.
Zero setup on subsequent runs.

## Troubleshooting

**"sudo: containerd: command not found"** — run `./demo/kilocode/run-demo.sh`
again. The script handles the containerd setup.

**"vsock unavailable" warning** — your kernel is missing the `vhost_vsock`
module. Load it with `sudo modprobe vhost_vsock`, then restart Bunkerbox.

**"bunkerbox-vscomm: installed 0 passthrough commands"** — the auto-detection
didn't find a build system, or your `passthrough` list is empty. Edit
`.bunkerbox/env.conf` manually and add your tools.

**Build failures inside the AI session** — the passthrough only proxies
whitelisted commands. If the AI tries `rustup` or `apt` and those aren't
whitelisted (and not in the VM), the command fails. Add them to the whitelist
if needed.

## Where next

- [Passthrough guide](passthrough.md) — understand the vsock proxy
- [Persistence guide](persistence.md) — save tool state between sessions
- [Runtime config](../config/runtime.md) — customize workspace, home, and network
