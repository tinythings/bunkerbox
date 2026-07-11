# Networking

Networking is controlled by runtime config.

Example:

```yaml
network: bridge
allow:
  - api.deepseek.com
```

## Bridge mode

```yaml
network: bridge
```

Bridge mode uses CNI bridge networking.

## Host mode

```yaml
network: host
```

Host mode uses host networking.

## Allow list

`allow` limits network destinations in bridge mode.

Example:

```yaml
allow:
  - api.deepseek.com
```

## Setup

Prepare host runtime:

```sh
make setup
```

Build and import an image:

```sh
make image IMAGE=images/opencode.conf
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

## DNS

When networking is enabled, Bunkerbox writes a container resolver config from the host resolver config.

Localhost nameservers are skipped.
