# Persistence

Persistence means the tool keeps its own app home between runs.

This is useful because many developer tools store config, session files, history, indexes, or login state under their home directory. Bunkerbox does not point those tools at your real home. It gives them a separate home and saves that home only when the runtime config asks for it.

Enable persistence in runtime config with:

```yaml
home: persist
```

## Where data is stored

By default, persisted home data is stored under the user data directory:

```text
$XDG_DATA_HOME/bunkerbox/<app>/home
```

If `XDG_DATA_HOME` is not set, Bunkerbox uses:

```text
~/.local/share/bunkerbox/<app>/home
```

For a command named `opencode`, that becomes:

```text
~/.local/share/bunkerbox/opencode/home
```

## What happens inside the container

The host persistence directory is mounted into the container at:

```text
/bunkerbox-persist-home
```

The generated entrypoint simply points the app at this directory:

```sh
export HOME=/bunkerbox-persist-home
```

No copies, no loop mounts, no crash recovery happen inside the guest. The entrypoint is trivial — all session management is on the host.

## Session image (host side)

Before the VM starts, Bunkerbox creates a loop-mounted ext4 image file on the host. That image is then bind-mounted into the VM at `/bunkerbox-persist-home`. The app writes directly to this mount, so writes go through the ext4 journal. If the VM crashes, the image file survives on the host disk and is recovered automatically on the next run.

The loop image lives at `.bunker/session.img` inside the persist home directory on the host.

```
┌─────────────────────────────────────────────────────────────────┐
│ HOST                                                            │
│                                                                 │
│ ~/.local/share/bunkerbox/<app>/home/  (raw persist home)        │
│   .bunker/session.img                 ← ext4 loop image         │
│   .config/ .local/share/ ...          ← populated from session  │
│                                      on teardown / recovery     │
│                                                                 │
│ sudo mount -o loop session.img                                  │
│         ▼                                                       │
│ /tmp/bunkerbox-session-<name>/  (loop mount, populated at setup)│
│   └── .config/ .local/share/ .cache/                            │
│         │  bind mount (virtio-fs)                               │
│         ▼                                                       │
└─────────────────────────────────────────────────────────────────┘
          │
          ▼
┌─────────────────────────────────────────────────────────────────┐
│ GUEST (Kata VM)                                                 │
│                                                                 │
│ /bunkerbox-persist-home  (bind mount of the loop mount)         │
│   └── .config/ .local/share/ .cache/                            │
│       ↑ app reads/writes here (ext4-backed, journaled via host) │
└─────────────────────────────────────────────────────────────────┘
```

**On startup:**
1. If a leftover `.bunker/session.img` exists from a crash: fsck, loop-mount it, copy newer files back to the raw persist home, unmount, delete
2. Create a fresh `session.img` with `dd` + `mke2fs`
3. Loop-mount it to a temp directory on the host
4. Copy the raw persist home into the loop mount
5. Bind-mount the loop mount into the VM at `/bunkerbox-persist-home`
6. The entrypoint sets `HOME=/bunkerbox-persist-home` and runs the app

**On exit (clean or error):**
1. Copy new and modified files from the loop mount back to the raw persist home (`cp -Rup`)
2. Unmount and delete `session.img`

**On crash:** `session.img` remains on the host disk. The next run recovers it.

## Session size limit

Set `session_mb` to cap the amount of data the app can write (default 50 MB):

```yaml
home: persist
session_mb: 50
```

Set `session_mb: 0` to disable the loop image entirely. The raw persist home is bind-mounted directly into the VM. No size cap, no crash recovery, no copy overhead on startup and exit.

## Encrypting secrets

Tools often store API keys and credentials in the persisted home. Add an `encrypt` list to the runtime config to protect those files:

```yaml
encrypt:
  - ".local/share/opencode/auth.json"
  - ".local/share/opencode/account.json"
```

Before the container starts, Bunkerbox prompts for a passphrase and decrypts any matching `.enc-cipher` files in the persisted home. The app sees plaintext. When the container exits, those files are re-encrypted to `.enc-cipher` and the plaintext is removed from host storage.

This happens entirely on the host side. The VM never sees crypto — the app inside the container works with plaintext as normal.

## Build and import

Persistence behavior is part of the generated image entrypoint. Build the image first:

```sh
make image IMAGE=images/opencode.conf
```

Then import the produced OCI archive for development use:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```
