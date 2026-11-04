#!/bin/sh
# Build the hand-made guest image for the libkrun spike (#16).
#
# Usage: scripts/vm-image.sh [construct-src] [out-dir]
#
# construct-src defaults to ../construct; out-dir defaults to target/vm.
# Produces <out>/base.img (host-arch Debian bookworm + Construct + socat) and
# <out>/journal.img (a separate ext4 disk for Construct's home). The guest
# must match the host architecture: aarch64 on macOS, x86_64 on Linux.
#
# Docker is a build tool here, not a runtime dependency. It must run native
# containers for the host. Nothing in this image is an environment pipeline:
# that is #10's job.
set -eu

construct_src=${1:-../construct}
out=${2:-target/vm}
arch=$(uname -m)
case "$arch" in
    arm64|aarch64) platform=linux/arm64 ;;
    x86_64|amd64) platform=linux/amd64 ;;
    *) echo "need an aarch64 or x86_64 host, got $arch" >&2; exit 1 ;;
esac
[ -d "$construct_src/crates/construct" ] || { echo "no construct at $construct_src" >&2; exit 1; }
command -v docker >/dev/null || { echo "docker is required to build the image" >&2; exit 1; }

construct_src=$(cd "$construct_src" && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/deck-vm-image.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

echo "building construct for $platform"
docker run --rm --platform "$platform" \
    -v "$construct_src:/src:ro" -v "$scratch:/out" -w /src rust:1.91-bookworm \
    bash -c 'set -e
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq pkg-config libdbus-1-dev >/dev/null
        CARGO_TARGET_DIR=/tmp/target cargo build --release -p construct
        cp /tmp/target/release/construct /out/construct'

# deck-egress is the guest half of the egress boundary (ADR 0004): the guest
# has no IP route, so loopback clients reach the host gateways through this
# relay. It is built for the guest architecture in the same native container
# shape as construct.
deck_src=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$scratch/egress/src"
cp -R "$deck_src/crates/deck-egress/src/." "$scratch/egress/src/"
cat > "$scratch/egress/Cargo.toml" <<'MANIFEST'
[package]
name = "deck-egress"
version = "0.0.1"
edition = "2024"

[dependencies]
libc = "0.2"
MANIFEST
echo "building deck-egress for $platform"
# A standalone copy of the crate, not the workspace: the workspace's [patch]
# entries would clone Zed's gpui git dependencies just to build this relay.
docker run --rm --platform "$platform" \
    -v "$scratch/egress:/src" -v "$scratch:/out" -w /src \
    -e CARGO_TARGET_DIR=/tmp/target rust:1.94-bookworm \
    bash -c 'cargo build --release && cp /tmp/target/release/deck-egress /out/deck-egress'

echo "collecting the Debian root for $platform"
cid=$(docker create --platform "$platform" debian:bookworm-slim bash -c '
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        bash ca-certificates coreutils socat util-linux procps git grep sed gawk \
        findutils tar gzip xz-utils less file >/dev/null
    rm -rf /var/lib/apt/lists/*')
docker start -a "$cid" >/dev/null
mkdir -p "$scratch/rootfs"
docker export "$cid" -o "$scratch/rootfs.tar"
docker rm "$cid" >/dev/null
tar -xf "$scratch/rootfs.tar" -C "$scratch/rootfs" --no-same-owner --no-xattrs

cp "$scratch/construct" "$scratch/rootfs/usr/local/bin/construct"
chmod 755 "$scratch/rootfs/usr/local/bin/construct"
cp "$scratch/deck-egress" "$scratch/rootfs/usr/local/bin/deck-egress"
chmod 755 "$scratch/rootfs/usr/local/bin/deck-egress"
cat > "$scratch/rootfs/usr/local/bin/deck-guest" <<'GUEST'
#!/bin/sh
# deck's guest entry: provision the workspace once, mount the journal disk,
# then hand ACP to the host over vsock. The guest opens the connection, so
# the connection is the readiness signal: no listener port, no fixed delay.
set -u
port=${1:-1024}
# /proc/uptime is free to read, unlike `date`, so the marks do not distort the
# launch they measure.
mark() { read -r up _ </proc/uptime; echo "deck-guest: $1 at ${up}s"; }
mark "start port=$port"
mkdir -p /workspace /mnt/import /root/.construct /tmp
if [ ! -e /workspace/.deck-provisioned ] && mount -t virtiofs import /mnt/import 2>/dev/null; then
  cp -a /mnt/import/. /workspace/ 2>/dev/null || true
  touch /workspace/.deck-provisioned
  umount /mnt/import
  mark "provisioned"
fi
if [ -b /dev/vdb ]; then
  if mount -t ext4 /dev/vdb /root/.construct 2>/dev/null; then
    mark "journal mounted"
  else
    echo "deck-guest: journal mount failed"
  fi
else
  echo "deck-guest: journal device missing"
fi
# DECK_SYNC=1 makes the flush measurement reproducible without a working
# shell tool in the guest. Normal runs leave it off and rely on SQLite.
if [ "${DECK_SYNC:-0}" = "1" ]; then
  ( while :; do sleep 1; sync; done ) &
fi
# The egress relay: loopback TCP -> the host's model gateway vsock port.
# It is a byte carrier, not a policy point; the host gateway enforces the
# model allowlist and the credential. A client that skips it meets the same
# gateway on the vsock port.
relay_port=${DECK_RELAY_PORT:-8080}
model_port=${DECK_MODEL_PORT:-1026}
boundary_port=${DECK_BOUNDARY_PORT:-8081}
extension_port=${DECK_EXTENSION_PORT:-1027}
boundary_kind=${DECK_BOUNDARY_KIND:-boundary}
web_port=${DECK_WEB_PORT:-8082}
mirror_port=${DECK_MIRROR_PORT:-8083}
web_vsock_port=${DECK_WEB_VSOCK_PORT:-1028}
mark "egress relay port=$model_port"
/usr/local/bin/deck-egress --listen "127.0.0.1:${relay_port}" --vsock "2:${model_port}" &
# The boundary status kind: plain HTTP on guest loopback, framed with the kind
# name before the host extension port carries it. Construct reads the base URL
# from the environment and advertises _meta.deck.boundary only when it is set.
/usr/local/bin/deck-egress --listen "127.0.0.1:${boundary_port}" --vsock "2:${extension_port}" --kind "${boundary_kind}" &
# The web and package gateway: one plain-HTTP relay for a program that reads
# `http_proxy`, and one for a mirror list. They are byte carriers like the
# model relay; the host gateway resolves, checks addresses and asks for a
# scope. A task without a web gateway gets neither relay and no proxy
# variable, because a proxy pointing at a closed port is worse than none.
if [ "${DECK_WEB_ENABLED:-0}" = "1" ]; then
  /usr/local/bin/deck-egress --listen "127.0.0.1:${web_port}" --vsock "2:${web_vsock_port}" &
  /usr/local/bin/deck-egress --listen "127.0.0.1:${mirror_port}" --vsock "2:${web_vsock_port}" &
  export http_proxy="http://127.0.0.1:${web_port}"
  export https_proxy="http://127.0.0.1:${web_port}"
  export HTTP_PROXY="$http_proxy" HTTPS_PROXY="$https_proxy"
  export no_proxy="127.0.0.1,localhost"
  export NO_PROXY="$no_proxy"
  export DECK_WEB_URL="http://127.0.0.1:${mirror_port}"
fi
# The GitHub operation gateway: a loopback relay the task's tool calls as
# DECK_GITHUB_URL. Like every relay it is a byte carrier; the host gateway
# enforces the repository and ref rules. A project with no repository gets no
# relay and no URL.
github_port=${DECK_GITHUB_PORT:-8084}
github_vsock_port=${DECK_GITHUB_VSOCK_PORT:-1029}
if [ "${DECK_GITHUB_ENABLED:-0}" = "1" ]; then
  /usr/local/bin/deck-egress --listen "127.0.0.1:${github_port}" --vsock "2:${github_vsock_port}" &
  export DECK_GITHUB_URL="http://127.0.0.1:${github_port}"
fi
export CONSTRUCT_BASE_URL="http://127.0.0.1:${relay_port}/v1"
export DECK_BOUNDARY_URL="http://127.0.0.1:${boundary_port}"
cd /workspace || cd /
# The guest opens the ACP stream to the host, so the connection itself is the
# readiness signal: no listener race, no fixed delay, and the agent starts
# while the host is still finishing the launch. Do not add socat's `stderr`
# option: it merges the agent's diagnostics into the ACP socket. They inherit
# the guest console instead, so the relay only ever carries protocol frames.
mark "connecting port=$port"
# The agent is the first address, so socat starts it immediately and only then
# connects the stream; its startup overlaps the vsock handshake.
exec /usr/bin/socat "SYSTEM:${DECK_AGENT} ${DECK_ARGS}" "VSOCK-CONNECT:2:${port}"
GUEST
chmod 755 "$scratch/rootfs/usr/local/bin/deck-guest"
# A readable file outside /workspace, for the permission-ask measurement.
echo outside > "$scratch/rootfs/root/outside.txt"
cat > "$scratch/rootfs/usr/local/bin/deck-flush" <<'FLUSH'
#!/bin/sh
# The flush measurement: one synced file, then a chunked unsynced stream.
echo FLUSH-START
echo synced > /workspace/synced.txt
sync
echo FLUSH-SYNCED
i=0
while [ "$i" -lt 64 ]; do
  dd if=/dev/zero bs=1M count=1 >> /workspace/stream.bin 2>/dev/null
  i=$((i+1))
  echo "CHUNK $i"
done
echo FLUSH-WRITTEN
sleep 300
FLUSH
chmod 755 "$scratch/rootfs/usr/local/bin/deck-flush"

# mkfs.ext4 -d cannot copy xattrs across a macOS bind mount, so the rootfs is
# unpacked inside the container from a tar.
tar -cf "$scratch/rootfs.tar" -C "$scratch/rootfs" --no-xattrs .
docker run --rm --platform "$platform" \
    -v "$scratch/rootfs.tar:/rootfs.tar:ro" -v "$out:/out" debian:bookworm-slim \
    bash -c 'set -e
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq e2fsprogs >/dev/null
        mkdir /rootfs
        tar -xf /rootfs.tar -C /rootfs
        truncate -s 6G /out/base.img
        mkfs.ext4 -q -F -L deck -d /rootfs /out/base.img
        mkdir -p /rootfs/root/.construct
        truncate -s 256M /out/journal.img
        mkfs.ext4 -q -F -L journal -d /rootfs/root/.construct /out/journal.img'

cat <<EOF
built $out/base.img and $out/journal.img

Point deckd at them. macOS also needs the Hypervisor entitlement
(scripts/vm-sign.sh); Linux needs the libkrunfw directory on LD_LIBRARY_PATH:
  export DECK_VM_IMAGE=$out/base.img
  export DECK_VM_JOURNAL=$out/journal.img
  export DECK_VM_KRUN=/path/to/libkrun.so.1.19.7   # .dylib on macOS
EOF
