#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"

if [ -n "${PROD:-}" ]; then
    BUILD_PROFILE="release"
    MAKE_TARGET="release"
else
    BUILD_PROFILE="debug"
    MAKE_TARGET="dev"
fi

SHARE_DIR="${SCRIPT_DIR}/share"
OCI_DIR="${SHARE_DIR}/oci"
KATA_DIR="${SHARE_DIR}/kata"
OCI_FILE="${OCI_DIR}/bunkerbox-netdebug-0.1.0.oci"
IMAGE_CONF="${SCRIPT_DIR}/netdebug-image.conf"
RUNTIME_CONF="${SHARE_DIR}/netdebug.conf"
BUNKERBOX="${PROJECT_ROOT}/target/${BUILD_PROFILE}/bunkerbox"
BUNKERBOX_IMAGE="${PROJECT_ROOT}/target/${BUILD_PROFILE}/bunkerbox-image"
NETDEBUG_LINK="${SCRIPT_DIR}/netdebug"
KATA_VERSION="${KATA_VERSION:-3.32.0}"
KATA_TARBALL_NAME="kata-static-${KATA_VERSION}-amd64.tar.zst"
KATA_TARBALL="${SCRIPT_DIR}/${KATA_TARBALL_NAME}"
KATA_SHARE_TARBALL="${SHARE_DIR}/${KATA_TARBALL_NAME}"
KATA_URL="https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/${KATA_TARBALL_NAME}"
KATA_ARCHIVE_PREFIX="/$(printf '%s/%s' opt kata)"
KATA_ARCHIVE_PATH=".${KATA_ARCHIVE_PREFIX}"

rm -f "$NETDEBUG_LINK" "$IMAGE_CONF"
rm -rf "$SHARE_DIR"
mkdir -p "$OCI_DIR" "$KATA_DIR"

make -C "$PROJECT_ROOT" "${MAKE_TARGET}"

if [ ! -f "$KATA_TARBALL" ]; then
  echo "Downloading Kata ${KATA_VERSION}..."
  curl -fL "$KATA_URL" -o "$KATA_TARBALL"
fi

cp "$KATA_TARBALL" "$KATA_SHARE_TARBALL"

echo "Installing Kata ${KATA_VERSION} to ${KATA_DIR}"
rm -rf "$KATA_DIR"
mkdir -p "$KATA_DIR"
tar --use-compress-program=unzstd -xf "$KATA_SHARE_TARBALL" -C "$KATA_DIR" --strip-components=3 "$KATA_ARCHIVE_PATH"
rm -f "$KATA_SHARE_TARBALL"

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
name: netdebug
image: localhost/bunkerbox-netdebug:0.1.0
output: ${OCI_FILE}
overwrite: true

files:
  - path: debug.sh
    mode: "0755"
    content: |
      #!/bin/sh
      set -u

      echo "== addresses =="
      ip addr || true
      echo

      echo "== routes =="
      ip route || true
      echo

      echo "== resolv.conf =="
      cat /etc/resolv.conf || true
      echo

      echo "== DNS: qwant.com =="
      nslookup qwant.com || true
      echo

      echo "== allowed: https://qwant.com =="
      curl -I --connect-timeout 10 https://qwant.com || true
      echo

      echo "== denied: https://google.com =="
      curl -I --connect-timeout 10 https://google.com || true
      echo

      echo "Interactive shell."
      exec /bin/sh

containerfile: |
  FROM docker.io/library/alpine:3.22

  RUN apk add --no-cache \
        bind-tools \
        busybox-extras \
        ca-certificates \
        curl \
        iproute2 \
        iputils \
        netcat-openbsd \
        openssl

  COPY debug.sh /usr/local/bin/debug.sh
  RUN chmod 0755 /usr/local/bin/debug.sh

  WORKDIR /workspace
  ENTRYPOINT ["/usr/local/bin/debug.sh"]
EOF_IMAGE

"$BUNKERBOX_IMAGE" "$IMAGE_CONF"

cat > "$RUNTIME_CONF" <<EOF_RUNTIME
oci: ${OCI_FILE}
image: localhost/bunkerbox-netdebug:0.1.0
workspace: share
home: persist
network: bridge
allow:
  - qwant.com
EOF_RUNTIME

ln -sfn "$BUNKERBOX" "$NETDEBUG_LINK"

cat <<EOF_DONE
Netdebug demo ready.
To rebuild with the production (release) version: PROD=1 ./run-demo.sh

Now run:
  cd ${SCRIPT_DIR}
  ./netdebug --share ./share

debug.sh runs automatically, then drops to a shell.
It checks:
  qwant.com works
  google.com is denied
EOF_DONE
