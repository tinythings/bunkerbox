# Packaging

Packaging is how a Bunkerbox tool becomes a normal command.

The user should be able to type something like:

```text
opencode
```

They should not need to know about image configs, OCI imports, or internal runtime setup. The package puts those pieces in the right places.

## How command names work

A packaged app command is a symlink to the Bunkerbox binary. When the user runs the command, Bunkerbox looks at the command name and loads a config with the same name.

If the command is:

```text
opencode
```

Bunkerbox loads:

```text
/usr/share/bunkerbox/opencode.conf
```

This is the key packaging idea. One Bunkerbox binary can power many app commands because each command name maps to a different runtime config.

## Package layout

A package installs runtime files under:

```text
/usr/share/bunkerbox
```

A typical layout looks like this:

```text
/usr/share/bunkerbox/
  opencode.conf
  oci/
    bunkerbox-opencode-1.17.18.oci
```

The runtime config points to the packaged OCI archive:

```yaml
oci: /usr/share/bunkerbox/oci/bunkerbox-opencode-1.17.18.oci
image: localhost/bunkerbox-opencode:1.17.18
```

The command symlink points to the Bunkerbox binary:

```text
/usr/bin/opencode -> /usr/bin/bunkerbox
```

When `/usr/bin/opencode` starts, Bunkerbox loads `opencode.conf`, imports the configured OCI archive, mounts the workspace, prepares home persistence, applies networking, and runs the tool.

## Building package assets

During development, build the OCI archive from an image config:

```sh
make image IMAGE=images/opencode.conf
```

The config decides the output archive name. The package then installs that archive under:

```text
/usr/share/bunkerbox/oci/
```

For local development, you can import an archive with:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

A real package should install the archive and runtime config so the command can run without the user manually passing those paths.
