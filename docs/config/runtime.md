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

## Network

With `network: bridge`, Bunkerbox uses bridge networking and can apply an allow list. With `network: host`, the tool uses host networking.

Use `allow` when you want bridge mode but only want specific destinations to be reachable.
