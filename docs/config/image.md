# Image config

An image config tells Bunkerbox how to build an OCI archive for a tool.

The config contains the image name, output archive name, app command, build arguments, hooks, optional extra files, and the container build recipe. You do not run the image builder directly. Use the Makefile:

```sh
make image IMAGE=images/opencode.conf
```

The `IMAGE` parameter is the path to the image config. You can point it at any config file under `images/`.

## Example

This is the important top part of the OpenCode image config:

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

The `output` field decides the OCI archive name. After building this config, the archive is written as:

```text
bunkerbox-opencode-1.17.18.oci
```

## The generated entrypoint

Every Bunkerbox image uses a generated entrypoint called:

```text
bunker-entrypoint
```

Your container recipe must copy it into the image and use it as the entrypoint:

```text
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

This generated entrypoint is important. It handles the session home lifecycle (creating a loop-mounted `session.img`, populating it from the persisted home, recovering from crashes), runs hooks, starts the app command, and preserves the app exit status.

## Fields

`name` is a short name for the image config. `image` is the local image tag used while building and importing. `output` is the OCI archive that will be written. `command` is the app command that runs inside the container. `containerfile` is the actual container build recipe.

Optional fields add behavior. `overwrite` allows replacing an existing archive. `build_args` passes values into the container build. `hooks` adds lifecycle shell snippets. `files` adds extra files to the build context. `runtime` auto-generates a runtime config file.

## Runtime section

The image author knows what runtime settings the tool needs (workspace mode, home persistence, network rules, and which files hold secrets). Define them once in the image config with the `runtime:` section, and the builder writes a runtime config automatically.

```yaml
runtime:
  workspace: cow
  home: persist
  network: bridge
  allow:
    - api.deepseek.com
  encrypt:
    - ".local/share/opencode/auth.json"
    - ".local/share/opencode/account.json"
```

When the builder finishes, it writes `<command>.conf` next to the OCI archive. The conf includes `oci` (the archive path) and `image` (the image tag) merged with everything from the `runtime:` section.

For the OpenCode config above, the builder would produce:

```text
bunkerbox-opencode-1.17.18.oci
opencode.conf
```

The packager installs both files. The `opencode.conf` is ready to use — no hand-editing needed.
