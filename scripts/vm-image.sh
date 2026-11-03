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
