# Custom images

A custom image config lets you package another tool for Bunkerbox.

Start by adding a config file under `images/`. The existing `images/opencode.conf` is the best example to copy from because it shows the expected shape: image name, output archive, command, hooks section, and container recipe.

Build a config with:

```sh
make image IMAGE=images/opencode.conf
```

For your own tool, replace the path with your new config file.

## What the config must do

The config must define the command that should run inside the container. It must also provide a container recipe that installs the tool.

The container recipe must copy the generated Bunkerbox entrypoint into the image:

```text
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
```

It must then use that generated file as the entrypoint:

```text
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

That entrypoint is what makes persistence and hooks work.

## Hooks

If the tool needs setup or cleanup, add hooks to the image config.

```yaml
hooks:
  before-app: |
    echo "starting app"
```

Hooks run inside the container, not on the host.

## Packaging the tool

After the image exists, packaging needs a runtime config and an app command. The runtime config tells Bunkerbox where the OCI archive is and how to run it. The app command is usually a symlink to the Bunkerbox binary.

Read [Packaging](packaging.md) for the full model.
