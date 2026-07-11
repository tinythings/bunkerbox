# Config schema

This page shows the current YAML shapes used by Bunkerbox.

## Image config

An image config lives under `images/`. It describes how to build one OCI archive.

```yaml
name: string
image: string
output: path
overwrite: bool
command:
  - string
build_args:
  NAME: value
hooks:
  before-home-load: shell
  before-app: shell
  after-app: shell
  app-error: shell
  after-home-save: shell
files:
  - path: relative/path
    mode: "0755"
    content: string
containerfile: string
```

Build an image config with:

```sh
make image IMAGE=images/opencode.conf
```

## Runtime config

A runtime config describes how a packaged command runs an image.

```yaml
oci: path
image: string
workspace: share | clone
home: persist | temporary
home_path: path
network: bridge | host
allow:
  - hostname
```

During development, runtime configs live in `runtime/`. In a packaged install, they live under:

```text
/usr/share/bunkerbox/
```

For a command named `opencode`, the packaged runtime config is:

```text
/usr/share/bunkerbox/opencode.conf
```

## Modes

`workspace` decides how the project is mounted. Use `share` for direct mounting and `clone` for a disposable workspace.

`home` decides whether app state is saved. Use `persist` to save state and `temporary` to throw it away after the run.

`network` decides how the container gets network access. Use `bridge` for isolated bridge networking and `host` for host networking.
