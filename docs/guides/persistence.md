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

The generated entrypoint uses a loop-mounted ext4 image file inside the guest for the app home. This provides crash-safe persistence: even if the VM is killed, data written before the crash survives in the image file and is recovered automatically on the next run.

The loop image lives at `.bunker/session.img` inside the persist home directory on the host.

```
┌─────────────────────────────────────────────────────────────────┐
│ HOST FILESYSTEM                                                 │
│ ~/.local/share/bunkerbox/<app>/home/                            │
│   .bunker/session.img  ← ext4 loop image (written via virtio-fs)│
│   ...                                                           │
│                                                                 │
│     virtio-fs bind mount                                        │
│         ▼                                                       │
│ /bunkerbox-persist-home (virtio-fs mount inside VM)             │
│         │                                                       │
│         │  populate on startup / sync back on exit              │
│         ▼                                                       │
│ /run/bunkerbox/session (loop mount of session.img)              │
│   └── .config/ .local/share/ .cache/                            │
│       ↑ app reads/writes here (ext4, journaled)                 │
└─────────────────────────────────────────────────────────────────┘
```

On startup:
1. If a leftover `.bunker/session.img` exists from a crash, it is fsck'd, mounted, and any newer files are copied back to the persist home
2. A fresh `session.img` is created with a size limit
3. The persist home is copied into the mounted image
4. The app runs with `HOME=/run/bunkerbox/session`

On exit:
1. New and modified files are copied back to the persist home
2. The image is unmounted and deleted

On crash: `session.img` remains on the host disk. The next run recovers it.

## Session size limit

Set `session_mb` to cap the amount of data the app can write (default 50 MB):

```yaml
home: persist
session_mb: 50
```

Set `session_mb: 0` to disable the loop image entirely. The app writes directly to the virtio-fs mount (`HOME=/tmp/bunkerbox-home`). This removes the size cap and crash recovery but avoids the copy overhead on startup and exit.

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
