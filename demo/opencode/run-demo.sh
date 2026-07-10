#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
SHARE_DIR="${SCRIPT_DIR}/share"
OCI_DIR="${SHARE_DIR}/oci"
KATA_DIR="${SHARE_DIR}/kata"
OCI_FILE="${OCI_DIR}/bunkerbox-opencode-1.17.18.oci"
IMAGE_CONF="${SCRIPT_DIR}/opencode-image.conf"
RUNTIME_CONF="${SHARE_DIR}/opencode.conf"
BUNKERBOX="${PROJECT_ROOT}/target/debug/bunkerbox"
BUNKERBOX_IMAGE="${PROJECT_ROOT}/target/debug/bunkerbox-image"
OPENCODE_LINK="${SCRIPT_DIR}/opencode"
KATA_VERSION="${KATA_VERSION:-3.32.0}"
KATA_TARBALL_NAME="kata-static-${KATA_VERSION}-amd64.tar.zst"
KATA_TARBALL="${SCRIPT_DIR}/${KATA_TARBALL_NAME}"
KATA_SHARE_TARBALL="${SHARE_DIR}/${KATA_TARBALL_NAME}"
KATA_URL="https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/${KATA_TARBALL_NAME}"
KATA_ARCHIVE_PREFIX="/$(printf '%s/%s' opt kata)"
KATA_ARCHIVE_PATH=".${KATA_ARCHIVE_PREFIX}"

rm -f "$OPENCODE_LINK" "$IMAGE_CONF"
rm -rf "$SHARE_DIR"
mkdir -p "$OCI_DIR" "$KATA_DIR"

make -C "$PROJECT_ROOT" dev

if [ ! -x "${KATA_DIR}/bin/containerd-shim-kata-v2" ]; then
  if [ ! -f "$KATA_TARBALL" ]; then
    echo "Downloading Kata ${KATA_VERSION}..."
    curl -fsSL "$KATA_URL" -o "$KATA_TARBALL"
  fi

  cp "$KATA_TARBALL" "$KATA_SHARE_TARBALL"

  echo "Installing Kata ${KATA_VERSION} to ${KATA_DIR}"
  rm -rf "$KATA_DIR"
  mkdir -p "$KATA_DIR"
  tar --use-compress-program=unzstd -xf "$KATA_SHARE_TARBALL" -C "$KATA_DIR" --strip-components=3 "$KATA_ARCHIVE_PATH"
  rm -f "$KATA_SHARE_TARBALL"
fi

if grep -Rqs "$KATA_ARCHIVE_PREFIX" "$KATA_DIR"; then
  echo "Updating Kata config paths for ${KATA_DIR}"
  grep -RIl "$KATA_ARCHIVE_PREFIX" "$KATA_DIR" \
    | xargs -r sed -i "s#${KATA_ARCHIVE_PREFIX}#${KATA_DIR}#g"
fi

QEMU_BIOS="${KATA_DIR}/share/kata-qemu/qemu/bios-256k.bin"
if [ -f "$QEMU_BIOS" ]; then
  sed -i "s#^firmware = \"\"#firmware = \"${QEMU_BIOS}\"#" \
    "${KATA_DIR}/share/defaults/kata-containers/configuration.toml" \
    "${KATA_DIR}/share/defaults/kata-containers/configuration-qemu.toml"
fi

QEMU_BIN="${KATA_DIR}/bin/qemu-system-x86_64"
QEMU_REAL="${KATA_DIR}/bin/qemu-system-x86_64.real"
QEMU_DATA_DIR="${KATA_DIR}/share/kata-qemu/qemu"
if [ -x "$QEMU_BIN" ] && [ ! -x "$QEMU_REAL" ]; then
  mv "$QEMU_BIN" "$QEMU_REAL"
  cat > "$QEMU_BIN" <<EOF_QEMU
#!/usr/bin/env sh
exec "$QEMU_REAL" -L "$QEMU_DATA_DIR" "\$@"
EOF_QEMU
  chmod 0755 "$QEMU_BIN"
fi

BUNKERBOX_KATA_DIR="$KATA_DIR" "$BUNKERBOX" setup

cat > "$IMAGE_CONF" <<EOF_IMAGE
name: opencode
image: localhost/bunkerbox-opencode:1.17.18
output: ${OCI_FILE}
overwrite: true

build_args:
  OPENCODE_VERSION: "1.17.18"

files:
  - path: bunker-entrypoint
    mode: "0755"
    content: |
      #!/bin/sh
      set -eu
      exec opencode

containerfile: |
  FROM docker.io/library/alpine:3.22

  ARG OPENCODE_VERSION

  RUN apk add --no-cache \\
        bash \\
        ca-certificates \\
        curl \\
        git \\
        libstdc++ \\
        openssh-client \\
        ripgrep \\
      && curl -fsSL \\
        "https://github.com/anomalyco/opencode/releases/download/v\${OPENCODE_VERSION}/opencode-linux-x64-baseline-musl.tar.gz" \\
        -o /tmp/opencode.tar.gz \\
      && tar -xzf /tmp/opencode.tar.gz -C /usr/local/bin opencode \\
      && chmod 0755 /usr/local/bin/opencode \\
      && rm -f /tmp/opencode.tar.gz \\
      && opencode --version

  RUN addgroup -g 1000 opencode \\
      && adduser -D -u 1000 -G opencode -s /bin/bash opencode \\
      && mkdir -p /workspace /home/opencode \\
      && chown -R opencode:opencode /workspace /home/opencode

  COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
  RUN chmod 0755 /usr/local/bin/bunker-entrypoint

  ENV HOME=/home/opencode \\
      XDG_CONFIG_HOME=/home/opencode/.config \\
      XDG_DATA_HOME=/home/opencode/.local/share \\
      XDG_STATE_HOME=/home/opencode/.local/state \\
      XDG_CACHE_HOME=/home/opencode/.cache

  USER opencode
  WORKDIR /workspace
  ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
EOF_IMAGE

"$BUNKERBOX_IMAGE" "$IMAGE_CONF"

cat > "$RUNTIME_CONF" <<EOF_RUNTIME
oci: ${OCI_FILE}
image: localhost/bunkerbox-opencode:1.17.18
EOF_RUNTIME

ln -sfn "$BUNKERBOX" "$OPENCODE_LINK"

cat <<EOF_DONE
Demo ready.

Now run:
  cd ${SCRIPT_DIR}
  ./opencode --share ./share
EOF_DONE
