# Runtime config

A runtime config tells Bunkerbox how to run a prepared image.

The image config answers "how do we build the tool image?" The runtime config answers "how should this tool run on this machine?" In a packaged install, runtime configs live under `/usr/share/bunkerbox/`.

## Example

The OpenCode runtime config looks like this:

```yaml
oci: /usr/share/bunkerbox/oci/bunkerbox-opencode-1.17.18.oci
image: localhost/bunkerbox-opencode:1.17.18
workspace: cow
workspace_quota: 10G
workspace_exclude:
  - demo/
home: persist
network: bridge
allow:
  - api.deepseek.com
```

The `oci` field points to the archive in the packaged install. The `image` field is the image tag used by the runtime. The `workspace` field controls how the project is mounted (see below). The `workspace_quota` field caps the upper layer size. The `workspace_exclude` field lists directory patterns to exclude from the overlay walk — these go through uncapped bind-mounts instead. The `home` field controls whether app state is saved. The `network` and `allow` fields control networking.

## Workspace

Bunkerbox supports three workspace modes:

### `cow` (copy-on-write, default)

Uses overlayfs with a loopback ext4 image (stored at `.bunkerbox/upper.img` in the repository root). The VM sees a merged view of the original repository (read-only lower layer) plus any writes (landing in the capped upper layer). This protects the host from disk exhaustion — the VM can write at most the quota bytes.

By default, the quota is auto-computed by walking the repository (skipping excluded directories) and adding 10% with a 1 GB floor. Build directories are excluded from the walk and bind-mounted to uncapped host storage under `.bunkerbox/build-workspace/`.

Per-project settings live in `.bunkerbox/env.conf` (auto-generated on first run). Edit this file to set an explicit quota or customize the exclude list.

```yaml
workspace: cow
workspace_quota: 10G     # optional, fallback when env.conf has no explicit quota
workspace_exclude:       # optional, fallback exclude patterns
  - target/
```

The old name `share` is accepted as an alias.

### `direct` (no guardrails)

The current project directory is mounted directly at `/workspace` inside the container. The VM can write unlimited data to the host filesystem. Use only for trusted workloads.

```yaml
workspace: direct
```

### `isolated` (full copy)

Bunkerbox prepares `.bunker/workspace` as a disposable clone of the project (via `git worktree` if possible, recursive copy otherwise). The original project is untouched. The old name `clone` is accepted as an alias.

```yaml
workspace: isolated
```

## Home

With `home: persist`, the tool keeps its app home between runs. This is useful for config, sessions, and tool state.

With `home: temporary`, the app home is not saved between runs.

When persistence is enabled, `session_mb` controls how the session home is stored:

```yaml
home: persist
session_mb: 50     # loop-mounted ext4 image (default 50), crash-safe
```

The entrypoint creates a loop-mounted ext4 image at `/run/bunkerbox/session` from `.bunker/session.img` inside the persisted home directory. All app writes go through the ext4 journal. If the VM crashes, the image file survives on the host disk and is recovered automatically on the next run.

Set `session_mb: 0` to disable the loop mount. The app writes directly to the virtio-fs bind mount. This removes the size cap and crash recovery but avoids the copy overhead on startup and exit.

For full details, see [Persistence](../guides/persistence.md).

## Encryption

The `encrypt` field lists file paths (relative to the persisted home directory) that contain secrets. Before the container starts, Bunkerbox prompts for a passphrase. Any `.enc-cipher` files in the persisted home are decrypted in place. When the container exits, files matching the encrypt patterns are encrypted to `.enc-cipher` and the plaintext is removed.

```yaml
encrypt:
  - ".local/share/opencode/auth.json"
  - ".local/share/opencode/account.json"
```

Passphrase can be supplied via environment variable to skip the prompt:

```sh
export BUNKERBOX_ENCRYPT_KEY="my-passphrase"
```

Files are encrypted with AES-256-GCM. The key is derived from the passphrase using PBKDF2-HMAC-SHA256 (100,000 iterations) with a random salt per file.

If the passphrase is wrong at startup, Bunkerbox prompts per-file:

```
decryption failed — wrong passphrase?: .local/share/opencode/auth.json.enc-cipher
Remove it? [y/N]
```

Answer `y` to delete the undecryptable file (the app will recreate it fresh) or `n` to abort.

## Network

With `network: bridge`, Bunkerbox uses bridge networking and can apply an allow list. With `network: host`, the tool uses host networking.

Use `allow` when you want bridge mode but only want specific destinations to be reachable.
