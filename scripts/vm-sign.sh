#!/bin/sh
# Codesign a deckd with Hypervisor.framework access for the libkrun spike.
#
# Usage: scripts/vm-sign.sh [binary]
#
# macOS refuses hv_vm_create() to a process without the hypervisor
# entitlement; the failure surfaces as a bare EINVAL from libkrun. Re-sign
# after every rebuild, and point DECK_VM_EXECUTABLE at a copy that cargo does
# not touch if the test binary and deckd share target/.
set -eu
binary=${1:-target/debug/deckd}
[ -f "$binary" ] || { echo "no $binary; build it first" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
codesign --entitlements "$here/hvf-entitlements.plist" --force -s - "$binary"
codesign -d --entitlements - "$binary" 2>&1 | grep -q com.apple.security.hypervisor ||
    { echo "the hypervisor entitlement did not stick" >&2; exit 1; }
