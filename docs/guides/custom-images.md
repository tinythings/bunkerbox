# Creating a custom image

Packaging a new tool for Bunkerbox means writing an image config. It's a YAML file that describes your tool, the container it runs in, and how it should behave at runtime.

## Start from an example

Copy one of the existing configs from `images/`. The simplest one to start from is the kilocode config:

```sh
cp images/kilocode.conf images/my-tool.conf
```

## The config structure

Every image config needs these parts:

| Section | What it does |
|---|---|
| `name`, `image`, `output` | Identifies the tool and names the output archive |
| `command` | The command that runs inside the container |
| `containerfile` | The recipe that installs your tool |
| `runtime` | Settings for how the tool runs on the user's machine |

## The container recipe

Your container recipe (`containerfile`) must install your tool and set up the Bunkerbox runtime. Here's the pattern every mature image uses:

```dockerfile
FROM docker.io/library/alpine:3.22

ARG MY_TOOL_VERSION

# Install your tool and its dependencies
RUN apk add --no-cache bash ca-certificates curl git \
      && curl -fsSL "https://example.com/my-tool-linux-musl.tar.gz" \
        -o /tmp/my-tool.tar.gz \
      && tar -xzf /tmp/my-tool.tar.gz -C /usr/local/bin \
      && chmod 0755 /usr/local/bin/my-tool-app \
      && rm -f /tmp/my-tool.tar.gz

# Required Bunkerbox directories
RUN mkdir -p /workspace /home/bunkerbox /usr/local/bunkerbox/bin \
      && chmod 0777 /workspace /home/bunkerbox /usr/local/bunkerbox/bin

# Required Bunkerbox files
COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
RUN chmod 0755 /usr/local/bin/bunker-entrypoint
COPY bunkerbox-vscomm /usr/local/bunkerbox/bin/bunkerbox-vscomm
COPY bunkerbox-status /usr/local/bunkerbox/bin/bunkerbox-status

ENV HOME=/home/bunkerbox
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
```

Key points:
- Use an `x86_64` musl-based image like `alpine:3.22`
- Copy `bunker-entrypoint` and set it as `ENTRYPOINT`
- Copy `bunkerbox-vscomm` and `bunkerbox-status` — they go in `/usr/local/bunkerbox/bin/`
- Create `/workspace` and `/home/bunkerbox` with write permissions

## Adding hooks

Hooks run shell commands at specific points during your tool's lifecycle:

```yaml
hooks:
  before-app: |
    bunkerbox-status status set 'Launching my-tool...'
    bunkerbox-status popup hide "SEC_2"
```

During startup, the status overlay shows "Launching my-tool..." and then fades after 2 seconds while your tool is already running.

For more details on hooks and what commands you can run, see [Hooks](../config/hooks.md).

## Packaging

After building, you'll have two files: the OCI archive and a runtime config. Both go into the system package. The runtime config tells Bunkerbox how to run your tool — workspace mode, network settings, which files to encrypt. Users invoke your tool through a symlink that points at the Bunkerbox binary.

Read [Packaging](packaging.md) for the full distribution model.
