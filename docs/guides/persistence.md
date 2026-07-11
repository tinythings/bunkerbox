# Persistence

Persistence saves app home data between runs.

Enable it in runtime config:

```yaml
home: persist
```

## Host path

Default host path:

```text
$XDG_DATA_HOME/bunkerbox/<app>/home
```

If `XDG_DATA_HOME` is not set:

```text
~/.local/share/bunkerbox/<app>/home
```

For command `opencode`:

```text
~/.local/share/bunkerbox/opencode/home
```

## Container path

The host home store is mounted at:

```text
/bunkerbox-persist-home
```

The generated entrypoint copies it into:

```text
/tmp/bunkerbox-home
```

The app runs with:

```text
HOME=/tmp/bunkerbox-home
```

## Flow

```text
host persisted home
copy into container temp home
app writes state
copy temp home back to host
```

## Build image with persistence support

Persistence support is in the generated entrypoint.

Build any image config:

```sh
make image IMAGE=images/opencode.conf
```

Import the resulting OCI archive:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```
