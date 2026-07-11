# Config schema

Current YAML fields.

## Image config

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

Build with:

```sh
make image IMAGE=images/opencode.conf
```

## Runtime config

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

Bundled runtime config:

```text
runtime/opencode.conf
```

Packaged runtime config path:

```text
/usr/share/bunkerbox/opencode.conf
```

## File modes

File modes can be strings or integers.
