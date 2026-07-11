# Runtime config

Runtime configs describe how a packaged command runs an OCI image.

Bundled runtime config:

```text
runtime/opencode.conf
```

Packaged location:

```text
/usr/share/bunkerbox/opencode.conf
```

## Real bundled config

```yaml
oci: /usr/share/bunkerbox/oci/bunkerbox-opencode-1.17.18.oci
image: localhost/bunkerbox-opencode:1.17.18
workspace: share
home: persist
network: bridge
allow:
  - api.deepseek.com
```

## Fields

| Field | Meaning |
|---|---|
| `oci` | Path to packaged OCI archive |
| `image` | Image tag imported into containerd |
| `workspace` | Workspace mode |
| `home` | Home mode |
| `home_path` | Optional custom persistent home path |
| `network` | Network mode |
| `allow` | Allowed network destinations |

## Workspace modes

| Mode | Meaning |
|---|---|
| `share` | Mount current project directly |
| `clone` | Use `.bunker/workspace` |

## Home modes

| Mode | Meaning |
|---|---|
| `persist` | Save app home between runs |
| `temporary` | Do not persist app home |

## Network modes

| Mode | Meaning |
|---|---|
| `bridge` | Use bridge networking |
| `host` | Use host networking |
