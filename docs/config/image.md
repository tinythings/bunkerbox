# Image config

An image config is a YAML file that tells Bunkerbox how to package your tool into a container image. You put these under the `images/` directory.

To build one:

```sh
make image IMAGE=images/my-tool.conf
```

## A minimal example

Here is the top of the OpenCode image config:

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

The `output` field names the archive. After building, you get `bunkerbox-opencode-1.17.18.oci`.

## Required fields

| Field | Purpose |
|---|---|
| `name` | Short name for the config |
| `image` | Tag used during build and import |
| `output` | Path to the OCI archive to create |
| `command` | The command that runs inside the container |
| `containerfile` | The Dockerfile/Podmanfile recipe |

## Optional fields

| Field | Purpose |
|---|---|
| `overwrite` | If `true`, replaces an existing archive instead of erroring |
| `build_args` | Key-value pairs passed to the container build |
| `hooks` | Shell commands that run at specific points (see [Hooks](hooks.md)) |
| `files` | Extra files to copy into the build context |
| `runtime` | Auto-generates a runtime config file |

## Container recipe

Your container recipe needs four things:

**1. A musl base image:**

```
FROM docker.io/library/alpine:3.22
```

**2. The generated entrypoint script:**

```
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
RUN chmod 0755 /usr/local/bin/bunker-entrypoint
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

**3. The Bunkerbox helper binaries:**

```
COPY bunkerbox-vscomm /usr/local/bunkerbox/bin/bunkerbox-vscomm
COPY bunkerbox-status /usr/local/bunkerbox/bin/bunkerbox-status
```

**4. A directory for the workspace and home:**

```
RUN mkdir -p /workspace /home/bunkerbox /usr/local/bunkerbox/bin \
    && chmod 0777 /workspace /home/bunkerbox /usr/local/bunkerbox/bin
```

## Runtime settings

Add a `runtime:` section to define how the tool should run on the user's machine:

```yaml
runtime:
  workspace: cow
  home: persist
  network: bridge
  allow:
    - api.deepseek.com
  encrypt:
    - ".local/share/my-app/auth.json"
```

The builder writes a `<command>.conf` file next to the archive, merging your `runtime:` settings with the archive path and image tag. For the OpenCode example above, you get:

```
bunkerbox-opencode-1.17.18.oci
opencode.conf
```

Both files go into the package install. No hand-editing needed. See [Runtime config](runtime.md) for all options.
