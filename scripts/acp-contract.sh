#!/usr/bin/env bash
# Verify or refresh the vendored copy of the shared ACP extension contract.
#
#   scripts/acp-contract.sh verify
#   scripts/acp-contract.sh update 1.0.0
#
# `update` needs `gh` authenticated for the contract repo; `verify` is offline
# and is what CI runs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/CONTRACT.lock"
field() { sed -n "s/^$1 = \"\(.*\)\"\$/\1/p" "$lock"; }
hash_tree() {
    python3 - "$1" <<'PY'
import hashlib, pathlib, sys
d = pathlib.Path(sys.argv[1])
h = hashlib.sha256()
for p in sorted(x for x in d.rglob('*') if x.is_file()):
    h.update(str(p.relative_to(d)).encode()); h.update(b'\0')
    h.update(p.read_bytes()); h.update(b'\0')
print(h.hexdigest())
PY
}
die() { echo "acp-contract: $*" >&2; exit 1; }

version=$(field version)
repo=$(field repo)
vendored="$root/$(field vendored)"

case "${1:-verify}" in
    verify)
        [ -n "$version" ] || die "CONTRACT.lock names no version"
        got=$(cat "$vendored/VERSION")
        [ "$got" = "$version" ] || die "vendored VERSION $got != lock $version"
        want=$(field sha256)
        have=$(hash_tree "$vendored")
        [ "$have" = "$want" ] || die "vendored tree $have != lock $want"
        echo "acp-contract $version ok ($have)"
        ;;
    update)
        new=${2:?usage: scripts/acp-contract.sh update <version>}
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        gh api "repos/$repo/tarball/v$new" | tar -xz -C "$tmp" --strip-components=1
        [ "$(cat "$tmp/VERSION")" = "$new" ] || die "release v$new VERSION disagrees"
        rm -rf "$vendored"
        mkdir -p "$vendored/schema"
        cp "$tmp/VERSION" "$tmp/conformance/matrix.json" "$vendored/"
        cp "$tmp"/schema/*.json "$vendored/schema/"
        commit=$(gh api "repos/$repo/commits/v$new" --jq .sha)
        hash=$(hash_tree "$vendored")
        cat > "$lock" <<LOCK
# The shared Deck-Construct ACP extension contract (docs/adr/0006).
# Source of truth: https://github.com/$repo
# Verify with \`scripts/acp-contract.sh verify\`; refresh with
# \`scripts/acp-contract.sh update <version>\`.
repo = "$repo"
version = "$new"
commit = "$commit"
vendored = "$(field vendored)"
sha256 = "$hash"
LOCK
        echo "acp-contract updated to $new ($commit, $hash)"
        ;;
    *)
        die "usage: scripts/acp-contract.sh [verify|update <version>]"
        ;;
esac
