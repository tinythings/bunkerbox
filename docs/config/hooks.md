# Image hooks

Hooks let you run shell commands at specific points during your app's lifecycle inside the container. They're defined in your image config.

## When hooks run

Hooks fire in this order:

```
container starts
├─ before-home-load   ← before loading your saved home directory
├─ before-app         ← right before your app starts
├─ your app runs
├─ after-app          ← after your app exits (always)
├─ app-error          ← only if your app failed (exit code ≠ 0)
├─ after-home-save    ← before saving your home directory
container stops
```

## Showing status messages

During startup, Bunkerbox shows a status overlay while the container boots. You can update what it says from inside a hook using the `bunkerbox-status` command:

```yaml
hooks:
  before-home-load: |
    bunkerbox-status status set 'Loading saved state...'

  before-app: |
    bunkerbox-status status set 'Starting my-app...'
```

The overlay hides automatically when your app produces its first output. If you want it to fade with a delay instead, use the `popup hide` command with a `SEC_` argument:

```yaml
hooks:
  before-app: |
    bunkerbox-status status set 'Launching my-app...'
    bunkerbox-status popup hide "SEC_2"
```

This shows "Launching my-app..." and then fades the overlay after 2 seconds, while your app is already running.

## Practical examples

Set up Git for the workspace:

```yaml
hooks:
  before-app: |
    git config --global --add safe.directory /workspace
```

Clean up cache after the app exits:

```yaml
hooks:
  after-app: |
    rm -rf "$HOME/.cache"
```

Show an error message when the app crashes:

```yaml
hooks:
  app-error: |
    bunkerbox-status popup info 'Error' 'The app exited unexpectedly'
```

## Available paths

When persistent home is enabled, your hooks can use these paths:

```
HOME=/bunkerbox-persist-home
XDG_CONFIG_HOME=$HOME/.config
XDG_DATA_HOME=$HOME/.local/share
XDG_STATE_HOME=$HOME/.local/state
XDG_CACHE_HOME=$HOME/.cache
```

The app's exit code is available in the `BUNKERBOX_APP_STATUS` variable inside `after-app` and `app-error` hooks.

Empty hooks are fine — they simply do nothing. Hooks use `/bin/sh`.
