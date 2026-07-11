# Persistence

Persistence means the tool keeps its own app home between runs.

This is useful because many developer tools store config, session files, history, indexes, or login state under their home directory. Bunkerbox does not point those tools at your real home. It gives them a separate home and saves that home only when the runtime config asks for it.

Enable persistence in runtime config with:

```yaml
home: persist
```

## Where data is stored

By default, persisted home data is stored under the user data directory:

```text
$XDG_DATA_HOME/bunkerbox/<app>/home
```

If `XDG_DATA_HOME` is not set, Bunkerbox uses:

```text
~/.local/share/bunkerbox/<app>/home
```

For a command named `opencode`, that becomes:

```text
~/.local/share/bunkerbox/opencode/home
```

## What happens inside the container

The host persistence directory is mounted into the container at:

```text
/bunkerbox-persist-home
```

The generated entrypoint copies that data into a temporary home:

```text
/tmp/bunkerbox-home
```

The app then runs with:

```text
HOME=/tmp/bunkerbox-home
```

When the app exits, Bunkerbox copies the temporary home back to the persisted home directory.

In plain words: the app writes to a container-local home while it runs, and Bunkerbox saves that home after the app is done.

## Build and import

Persistence behavior is part of the generated image entrypoint. Build the image first:

```sh
make image IMAGE=images/opencode.conf
```

Then import the produced OCI archive for development use:

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```
