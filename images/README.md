# Images

Image config files in this directory describe how to build OCI archives for tools running inside Bunkerbox.

Each `.conf` file contains the image name, app command, build arguments, hooks, and the container build recipe. See `docs/config/image.md` for the full field reference.

## Building an image

Use the Makefile:

```sh
make image IMAGE=images/opencode.conf
```

Or invoke the image builder directly:

```sh
target/debug/bunkerbox-image images/opencode.conf
```

The `output` field in the config decides the OCI archive name. To override it, pass `--output`:

```sh
target/debug/bunkerbox-image images/opencode.conf --output /custom/path/output.oci
```

After building, import the OCI archive into containerd:

```sh
make install-image OCI=path/to/image.oci
```

## Available image configs

| Config | Tool | Output archive |
|--------|------|---------------|
| `opencode.conf` | [OpenCode](https://github.com/anomalyco/opencode) | `bunkerbox-opencode-1.17.18.oci` |
| `kilocode.conf` | [KiloCode](https://github.com/Kilo-Org/kilocode) | `bunkerbox-kilocode-7.4.11.oci` |
| `crush.conf` | [Crush](https://github.com/charmbracelet/crush) | `bunkerbox-crush-0.84.1.oci` |
