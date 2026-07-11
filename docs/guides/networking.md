# Networking

Networking is controlled by runtime config.

Some tools need network access. Others should not talk to the network at all. Bunkerbox makes network behavior explicit so the runtime config says what kind of access the tool should get.

## Bridge mode

Bridge mode gives the container its own network path:

```yaml
network: bridge
```

When bridge mode is used, Bunkerbox can also apply an allow list. For example:

```yaml
allow:
  - api.deepseek.com
```

That means the runtime is intended to allow only specific destinations.

## Host mode

Host mode uses host networking:

```yaml
network: host
```

This is less isolated than bridge mode, but may be useful for tools that need to behave like they are running directly on the host network.

## DNS

When networking is enabled, Bunkerbox writes a resolver config for the container based on the host resolver config. Localhost nameservers are skipped because they usually do not work from inside the isolated container.

## Setup

Prepare the host runtime first:

```sh
make setup
```

Then build and import the image you want to run:

```sh
make image IMAGE=images/opencode.conf
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```
