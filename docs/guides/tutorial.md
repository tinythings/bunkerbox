# Tutorial

## What you'll learn

You have a Rust project and you want an AI coding agent to work on it. You
don't trust the agent on your real machine — it could `rm -rf` your home
directory, exfiltrate your SSH keys, or fill your disk with garbage. So you
put it inside Bunkerbox, a VM-level isolation layer.

But now the VM doesn't have `cargo`, `rustc`, or `rustup`. The AI can't build
anything. You don't want to bake a full Rust toolchain into the container image
either — that defeats the purpose of a small, immutable VM, and your project
might need Fedora toolchains while the VM runs Alpine.

This tutorial shows you how to solve that. By the end, you will have:

- A Bunkerbox setup that isolates an AI agent in a lightweight VM.
- A passthrough whitelist that lets the agent call `cargo build`, `cargo test`,
  and `cargo clippy` — commands that actually run on your host machine, inside
  an overlay copy of your project.
- An overlay workspace that captures everything the agent changes, then
  auto-syncs it back to your real repository when the container exits.
- A clean Git diff you can review and commit.

The AI never touches your real filesystem. All it can do is call the build
tools you whitelisted. All output lands in an isolated snapshot. You decide
what to keep.

## Prerequisites

An Ubuntu 22.04 or 24.04 machine with:

- Rust 1.97+ (`rustup` recommended)
- `make`, `curl`, `sudo`
- At least 8 GB free disk space

This tutorial uses a Rust project, so a working Rust toolchain must be present
on your host. The AI agent calls `cargo` through passthrough — it uses
**your** real `cargo`, **your** real `rustc`, and **your** real
`~/.cargo/registry`. Verify:

```sh
rustc --version
cargo --version
```

If you haven't installed Rust yet:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

---

## Step 1 — Get the code

Clone the repository. This gives you the Rust source, the demo scripts, and
the image configs.

```sh
git clone https://github.com/bunker-project/bunkerbox
cd bunkerbox
```

*We haven't built anything yet. The next step does the heavy lifting.*

## Step 2 — One-shot demo setup

This is the "make it go" step. It does everything Bunkerbox needs to run:
compiles the Rust binaries, downloads the VM runtime, builds the KiloCode
container image, and creates a `kilo` launcher.

```sh
./demo/kilocode/run-demo.sh
```

The first run downloads ~150 MB of runtime and builds the project from
source. On subsequent runs it finishes in seconds (cached).

Two things to note:
- **Passphrase prompt.** Press Enter twice for an empty passphrase (no
  credential encryption). You can set one later.
- **Wait for "Demo ready."** That's the all-clear signal.

*Why a script?* Because the setup chain is long: build binaries, download and
configure the VM runtime, compile the OCI image, create symlinks. The script
handles all of it. Nothing is installed system-wide — everything lives under
`demo/kilocode/share/`.

## Step 3 — Verify it works

```sh
cd demo/kilocode
./kilo --version
```

You should see `Bunkerbox x.y.z`. The `kilo` symlink points to your debug
build of `bunkerbox`. When invoked as `kilo`, it auto-detects the name and
loads the matching runtime config from `./share/kilo.conf`.

*If you don't see a version, Step 2 didn't finish. Re-run the demo script.*

## Step 4 — Create something for the AI to fix

In a fresh directory, create a tiny Rust project. This is the project the AI
will work on — isolated from Bunkerbox itself.

Your host `cargo` must be on `PATH`. The passthrough daemon inherits your host
shell environment — if you can run `cargo build` in a terminal, the AI can too.

```sh
mkdir ~/bunkerbox-tutorial
cd ~/bunkerbox-tutorial
cargo init --name hello
```

Now plant a deliberate mistake. Replace `src/main.rs` with:

```rust
fn main() {
    let numbers = vec![1, 2, 3, 4, 5];
    let total: i32 = numbers.iter().sum();
    println!("sum is {total}")
}
```

This won't compile. `Iterator::sum()` isn't in scope without `use
std::iter::Sum`. The AI needs to figure that out and fix it.

*Why a broken project?* So you can watch the AI actually do something —
compile, fail, read the error, fix the code, recompile, succeed. If the
project already builds, the AI has nothing to demonstrate.

## Step 5 — Prepare the workspace

Bunkerbox needs a workspace configuration. This step creates `.bunkerbox/` in
your project root and auto-detects what build tools you use.

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share prepare
```

Replace `/path/to/bunkerbox` with the actual path to the cloned repository.

Open `.bunkerbox/project.conf`. You should see something like:

```yaml
quota: auto
exclude:
  - target/
  - node_modules/
  - .venv/
  - ...
passthrough:
  - "cargo *"
```

The `passthrough` section is the key. `"cargo *"` means: "let the AI call
`cargo` with any arguments." Bunkerbox detected `Cargo.toml` in your project
root and added it automatically. If you had a `Makefile` too, `"make *"` would
also be there.

Verify it was detected:

```sh
grep -A2 passthrough .bunkerbox/project.conf
```

*Why `prepare`?* This step is like `npm install` or `cargo init` — it
bootstraps the project for Bunkerbox. It creates the overlay image, the
workspace directory, and the config file. It only needs to run once per
project.

## Step 6 — Let the AI work

Now the real thing. Launch KiloCode inside Bunkerbox, pointed at your tutorial
project:

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share
```

KiloCode starts. Give it a prompt:

> Please run `cargo build` to check this project. If it doesn't compile, fix
> the error and run `cargo build` again until it succeeds. Then run `cargo
> test`.

Watch what happens:

1. The AI calls `cargo build`. The call goes through the vsock passthrough —
   `cargo` runs on your host, inside `.bunkerbox/workspace/`, using your real
   Rust toolchain.
2. Compilation fails. The AI reads the error.
3. The AI edits `src/main.rs` inside the workspace, adds the missing import,
   and calls `cargo build` again.
4. This time it compiles. The AI runs `cargo test`.
5. Everything passes. The AI reports success.

The AI never touched your real `src/main.rs`. Everything happened inside the
overlay. Your original file is untouched until you choose to sync.

*Why passthrough?* Because the VM doesn't have a Rust toolchain. Installing one
inside the VM would bloat the image and pin you to one distro. Passthrough
gives the AI full build capabilities using whatever toolchain is on your host
machine, while keeping the VM itself minimal and immutable.

## Step 7 — See what the AI did

When KiloCode exits, Bunkerbox auto-syncs all changes from the overlay
workspace back to your real project directory. Check the results:

```sh
ls -la src/
cat src/main.rs
git diff
```

You'll see the AI's edits. They're now in your real working tree — just like
if you had made them yourself. Review the diff, stage it, commit it.

*Why auto-sync?* Without it, you'd have to manually run `bunkerbox sync` after
every session to pull changes out of the overlay. Auto-sync makes the
"AI worked on it → changes appear in my repo" flow instantaneous.

## Step 8 — Do it again

The workspace survives between sessions — the overlay upper layer persists,
the config is already written. Next time:

```sh
cd ~/bunkerbox-tutorial
/path/to/bunkerbox/demo/kilocode/kilo --share /path/to/bunkerbox/demo/kilocode/share
```

That's it. No `prepare`, no config editing, no demo script. Just run it.
If you ever need a fresh start, `bunkerbox prepare --reset` wipes the
workspace and re-creates it.

---

## What just happened — the big picture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Your host                                                           │
│                                                                      │
│  ~/bunkerbox-tutorial/           ← real repo (untouched)             │
│  ~/bunkerbox-tutorial/.bunkerbox/ ← overlay upper layer              │
│  .bunkerbox/workspace/            ← where AI actually writes         │
│                                                                      │
│  Bunkerbox daemon listens on vsock port 9999.                        │
│  When AI calls `cargo build`:                                        │
│    → checks whitelist ("cargo *" ✓)                                  │
│    → runs `cargo build` inside .bunkerbox/workspace/                 │
│    → streams stdout, stderr, exit code back                          │
│                                                                      │
├──────────────────────────────────────────────────────────────────────┤
│                        virtio-vsock                                  │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  Bunkerbox VM (Alpine, ~150 MB)                                      │
│                                                                      │
│  /workspace  → bind mount of .bunkerbox/workspace/                   │
│  /usr/local/bunkerbox/bin/cargo                                      │
│               → symlink → bunkerbox-vscomm                           │
│                                                                      │
│  KiloCode runs here. Calls `cargo build`.                            │
│  Sees real build output. Fixes code.                                 │
│  Never touches your real files.                                      │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

The VM is a disposable bubble. The passthrough is a pinhole — only the
commands you whitelist can get through. The overlay is a safety net — changes
are captured, never destructive. You are in control.

---

## Troubleshooting

**"sudo: containerd: command not found"**
Re-run `./demo/kilocode/run-demo.sh`. The script handles containerd setup.

**"vsock unavailable" warning**
Your kernel is missing the `vhost_vsock` module. Load it and restart:
```sh
sudo modprobe vhost_vsock
```

**"bunkerbox-vscomm: installed 0 passthrough commands"**
The auto-detection didn't find a build system, or your `passthrough` list is
empty. Edit `.bunkerbox/project.conf` and add your tools manually.

**Build failures inside the AI session**
The passthrough only proxies whitelisted commands. If the AI tries `rustup`,
`apt`, or anything not in the whitelist (and those commands don't exist in the
VM), the call fails. The agent gets "command not found" — which is correct
inside a bunker.

**The AI asks for tools you didn't whitelist**
Add them. Edit `.bunkerbox/project.conf`, add the command to `passthrough`, and
run the session again. The symlinks are created fresh on every boot.

---

## Where next

- [Passthrough guide](passthrough.md) — the full vsock proxy architecture
- [Persistence guide](persistence.md) — save AI tool state across sessions
- [Runtime config](../config/runtime.md) — workspace modes, networking, encryption
