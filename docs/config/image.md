# Image config

Image configs describe how to build OCI archives.

Build any image config with:

```sh
make image IMAGE=images/opencode.conf
```

`IMAGE` is the config path.

## Example config

```yaml
name: opencode
image: localhost/bunkerbox-opencode:1.17.18
output: bunkerbox-opencode-1.17.18.oci
overwrite: true
command:
  - opencode

build_args:
  OPENCODE_VERSION: "1.17.18"
```

## Fields

| Field | Required | Meaning |
|---|---:|---|
| `name` | yes | Short image config name |
| `image` | yes | Image tag used when building and importing |
| `output` | yes | OCI archive written by the build |
| `overwrite` | no | Replace existing OCI archive |
| `command` | yes | App command executed inside the container |
| `build_args` | no | Values passed into the Containerfile build |
| `hooks` | no | Shell snippets added to the generated entrypoint |
| `files` | no | Extra files added to the build context |
| `containerfile` | yes | Container build recipe |

## Generated entrypoint

The image build creates a generated file:

```text
bunker-entrypoint
```

The `containerfile` must copy it into the image:

```text
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
```

And use it as entrypoint:

```text
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

That entrypoint handles:

- persistent home copy in
- hooks
- app command
- persistent home copy out
