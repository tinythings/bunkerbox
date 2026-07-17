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

### Egress firewall

When both `network: bridge` and `allow` are set, Bunkerbox deploys iptables rules to enforce the allow list. The firewall:

1. Loads the `br_netfilter` kernel module and enables `net.bridge.bridge-nf-call-iptables` so bridged traffic passes through iptables.
2. Creates a chain `BUNKERBOX-EGRESS` and hooks it from the `FORWARD` chain by matching the bridge subnet.
3. Permits established and related return traffic.
4. Permits DNS to the host's configured resolvers (UDP and TCP port 53).
5. Resolves each hostname in the allow list to IPv4 addresses and permits traffic to those destinations.
6. Rejects all other egress.

The bridge subnet is `10.247.0.0/24`. Firewall rules are torn down when the container exits.

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
