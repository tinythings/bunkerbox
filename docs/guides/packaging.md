# Packaging

Packaging turns a Bunkerbox runtime into a normal command.

Example command:

```text
opencode
```

That command is a symlink to the Bunkerbox binary.

## Package layout

Default share directory:

```text
/usr/share/bunkerbox
```

Expected layout:

```text
/usr/share/bunkerbox/
  <command>.conf
  oci/
    <image>.oci
```

Example:

```text
/usr/share/bunkerbox/
  opencode.conf
  oci/
    bunkerbox-opencode-1.17.18.oci
```

## Runtime config name

The command name selects the runtime config.

If the command is:

```text
opencode
```

Bunkerbox loads:

```text
/usr/share/bunkerbox/opencode.conf
```

## OCI archive path

The runtime config points to the packaged OCI archive:

```yaml
oci: /usr/share/bunkerbox/oci/bunkerbox-opencode-1.17.18.oci
image: localhost/bunkerbox-opencode:1.17.18
```

## Build package assets

Build an OCI archive from an image config:

```sh
make image IMAGE=images/opencode.conf
```

The image config decides the output file.

For `images/opencode.conf`, the output is:

```text
bunkerbox-opencode-1.17.18.oci
```

A package places that archive under:

```text
/usr/share/bunkerbox/oci/
```

It also places the runtime config under:

```text
/usr/share/bunkerbox/
```

## Installed command

A package should install an app command symlink.

Example:

```text
/usr/bin/bunkerbox
/usr/bin/opencode -> /usr/bin/bunkerbox
```

When `/usr/bin/opencode` runs, Bunkerbox sees the invoked name `opencode` and loads `opencode.conf`.

## Development import

For development, import an OCI archive with:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

Packaging is different.
Packaging installs files into `/usr/share/bunkerbox` and provides app command symlinks.
