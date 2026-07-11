# Concepts

## Makefile workflow

Development tasks are Makefile targets.

Use `make` commands.

See [Makefile reference](reference/makefile.md).

## Image config

Image configs live in:

```text
images/
```

They describe how to build a prepared OCI archive.

Build one with:

```sh
make image IMAGE=images/opencode.conf
```

`IMAGE` is the image config path.

## Runtime config

Runtime configs live in:

```text
runtime/
```

They describe how a packaged command should run.

Example source config:

```text
runtime/opencode.conf
```

Packaged config path:

```text
/usr/share/bunkerbox/opencode.conf
```

## Workspace

The project workspace is mounted inside the container at:

```text
/workspace
```

Workspace modes:

| Mode | Meaning |
|---|---|
| `share` | Mount current project directly |
| `clone` | Use `.bunker/workspace` |

## Home persistence

When home persistence is enabled, Bunkerbox saves app home data on the host.

Inside the container, the app writes to a temporary home.

See [Persistence](guides/persistence.md).

## Image hooks

Hooks are shell snippets in image configs.

They run inside the container before or after app lifecycle steps.

See [Image hooks](config/hooks.md).

## Packaging

Packaging installs:

- Bunkerbox binary
- app command symlink
- runtime config
- OCI archive

See [Packaging](guides/packaging.md).
