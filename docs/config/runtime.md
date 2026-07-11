# Runtime config

A runtime config tells Bunkerbox how to run a prepared image.

The image config answers “how do we build the tool image?” The runtime config answers “how should this tool run on this machine?”

During development, runtime configs live in `runtime/`. In a packaged install, they live under `/usr/share/bunkerbox/`.

## Example

The OpenCode runtime config looks like this:

```yaml
oci: /usr/share/bunkerbox/oci/bunkerbox-opencode-1.17.18.oci
image: localhost/bunkerbox-opencode:1.17.18
workspace: share
home: persist
network: bridge
allow:
  - api.deepseek.com
```

The `oci` field points to the archive in the packaged install. The `image` field is the image tag used by the runtime. The `workspace` field controls how the project is mounted. The `home` field controls whether app state is saved. The `network` and `allow` fields control networking.

## Workspace

With `workspace: share`, the current project is mounted directly at `/workspace` inside the container. This is simple and fast.

With `workspace: clone`, Bunkerbox prepares `.bunker/workspace` first. That gives the tool a disposable workspace instead of the original project directory.

## Home

With `home: persist`, the tool keeps its app home between runs. This is useful for config, sessions, and tool state.

With `home: temporary`, the app home is not saved between runs.

## Network

With `network: bridge`, Bunkerbox uses bridge networking and can apply an allow list. With `network: host`, the tool uses host networking.

Use `allow` when you want bridge mode but only want specific destinations to be reachable.
