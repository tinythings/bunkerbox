# Image hooks

Image hooks are shell snippets in an image config.

They run inside the container.

Build an image containing hooks with:

```sh
make image IMAGE=images/opencode.conf
```

`IMAGE` can point to any image config.

## Supported hooks

| Hook | When it runs |
|---|---|
| `before-home-load` | Before persisted home is copied into temp home |
| `before-app` | Before the app starts |
| `after-app` | After the app exits |
| `app-error` | Only when the app exits non-zero |
| `after-home-save` | After temp home is copied back to persisted home |

## Flow

```text
container starts
before-home-load
copy persisted home to temp home
before-app
run app
after-app
app-error, only if app failed
copy temp home to persisted home
after-home-save
container exits
```

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

## Environment

When persistent home is enabled:

```text
BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home
HOME=/tmp/bunkerbox-home
XDG_CONFIG_HOME=/tmp/bunkerbox-home/.config
XDG_DATA_HOME=/tmp/bunkerbox-home/.local/share
XDG_STATE_HOME=/tmp/bunkerbox-home/.local/state
XDG_CACHE_HOME=/tmp/bunkerbox-home/.cache
```

After app exit:

```text
BUNKERBOX_APP_STATUS=<exit-code>
```

## Rules

- hooks use `/bin/sh`
- empty hooks do nothing
- `after-app` runs on success and failure
- `app-error` runs only on failure
