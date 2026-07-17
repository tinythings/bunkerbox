# Image hooks

Image hooks are small shell scripts stored in an image config. They run inside the container.

Hooks are useful when a tool needs a little setup before it starts or cleanup after it exits. For example, a hook can create config directories, mark the workspace as safe for Git, print diagnostics when the app fails, or remove cache before state is saved.

Build an image with hooks the same way you build any image config:

```sh
make image IMAGE=images/opencode.conf
```

## Hook order

The generated entrypoint runs hooks in this order:

```text
container starts
before-home-load
populate session home from persisted home
before-app
run app
after-app
app-error, only if app failed
sync session home back to persisted home
after-home-save
container exits
```

When `session_mb` is set (the default), the session home is a loop-mounted ext4 image at `/run/bunkerbox/session`. The entrypoint recovers any left-over `session.img` from a previous crash, creates a fresh image, copies the persist home into it, and syncs changes back on exit.

When `session_mb: 0`, the session home is a tmpfs at `/tmp/bunkerbox-home` and the old copy-in/copy-out behaviour applies.

`before-home-load` runs before the session home is populated. `before-app` runs right before the app command starts. `after-app` runs after the app exits, whether it succeeded or failed. `app-error` only runs when the app exits with a non-zero status. `after-home-save` runs after changes are synced back to persistent storage.

## Example

```yaml
hooks:
  before-app: |
    git config --global --add safe.directory /workspace

  after-app: |
    rm -rf "$HOME/.cache"

  app-error: |
    echo "app failed with status $BUNKERBOX_APP_STATUS"
```

The app exit code is available as:

```text
BUNKERBOX_APP_STATUS
```

When persistent home is enabled, hooks can also use these paths:

With `session_mb` (default):

```text
BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home
HOME=/run/bunkerbox/session
XDG_CONFIG_HOME=/run/bunkerbox/session/.config
XDG_DATA_HOME=/run/bunkerbox/session/.local/share
XDG_STATE_HOME=/run/bunkerbox/session/.local/state
XDG_CACHE_HOME=/run/bunkerbox/session/.cache
```

With `session_mb: 0`:

```text
BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home
HOME=/tmp/bunkerbox-home
XDG_CONFIG_HOME=/tmp/bunkerbox-home/.config
XDG_DATA_HOME=/tmp/bunkerbox-home/.local/share
XDG_STATE_HOME=/tmp/bunkerbox-home/.local/state
XDG_CACHE_HOME=/tmp/bunkerbox-home/.cache
```

Empty hooks do nothing. Hooks use `/bin/sh`.
