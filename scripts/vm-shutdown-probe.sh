#!/bin/sh
# Run only in a disposable guest: write or verify both disks, then attempt
# the unmount sequence a clean shutdown requires.
set -eu
mkdir -p /workspace /root/.construct
mount -t ext4 /dev/vdb /root/.construct
case "${1:-}" in
  write)
    printf 'workspace survives\n' > /workspace/deck-shutdown-probe
    printf 'journal survives\n' > /root/.construct/deck-shutdown-probe
    ;;
  verify)
    test "$(cat /workspace/deck-shutdown-probe)" = 'workspace survives'
    test "$(cat /root/.construct/deck-shutdown-probe)" = 'journal survives'
    echo DECK_DISKS_VERIFIED
    ;;
  *) echo 'expected write or verify' >&2; exit 1 ;;
esac
sync
umount /root/.construct
mount -o remount,ro /
echo DECK_UNMOUNT_CONFIRMED
echo o > /proc/sysrq-trigger
sleep 5
echo DECK_POWEROFF_FAILED >&2
exit 1
