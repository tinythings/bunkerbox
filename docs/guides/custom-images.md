# Custom images

Create an image config under:

```text
images/
```

Use the existing opencode config as the real example:

```text
images/opencode.conf
```

## Build a custom image config

```sh
make image IMAGE=images/opencode.conf
```

Replace `images/opencode.conf` with the config file you added.

## Required pieces

An image config needs:

- `name`
- `image`
- `output`
- `command`
- `containerfile`

The `containerfile` must copy the generated entrypoint:

```text
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
```

And use it:

```text
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

## Add hooks

```yaml
hooks:
  before-app: |
    echo "starting app"
```

Build again:

```sh
make image IMAGE=images/opencode.conf
```

## Package it

To package a custom image, add:

- OCI archive under `/usr/share/bunkerbox/oci/`
- runtime config under `/usr/share/bunkerbox/`
- app command symlink to Bunkerbox binary

See [Packaging](packaging.md).
