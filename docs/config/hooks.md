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
before-app
run app
after-app
app-error, only if app failed
after-home-save
container exits
```

All session management (creating the loop-mounted ext4 image, populating it from the persist home, recovering from crashes, syncing back on exit) happens on the host before the container starts and after it exits. The entrypoint inside the container is trivial — it just sets `HOME` and runs the app.

`before-home-load` runs before the app starts but after the session home is bind-mounted. `before-app` runs right before the app command starts. `after-app` runs after the app exits, whether it succeeded or failed. `app-error` only runs when the app exits with a non-zero status. `after-home-save` runs before the container exits (before the host-side sync back to the persist home).

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

When persistent home is enabled, hooks can reference these paths:

```text
BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home
HOME=/bunkerbox-persist-home
XDG_CONFIG_HOME=/bunkerbox-persist-home/.config
XDG_DATA_HOME=/bunkerbox-persist-home/.local/share
XDG_STATE_HOME=/bunkerbox-persist-home/.local/state
XDG_CACHE_HOME=/bunkerbox-persist-home/.cache
```

Empty hooks do nothing. Hooks use `/bin/sh`.
