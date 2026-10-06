#!/usr/bin/env bash
# Sign a draft release and publish it.
#
#   scripts/sign-release.sh v0.0.5
#
# CI builds the archives and leaves a draft; it has no signing key. This
# downloads that draft, checks GitHub attests CI built those exact files from
# the tag, waits while you try them, then signs SHA256SUMS.txt with your own
# key and publishes. Installed copies only update to releases signed this way;
# see src/update.rs.
#
# The key defaults to ~/.minisign/mnm-main.key; set MINISIGN_KEY to use the
# spare instead.
set -euo pipefail

repo=Noodle-face/mnm-cartographer
workflow=$repo/.github/workflows/release.yml
key=${MINISIGN_KEY:-$HOME/.minisign/mnm-main.key}
here=$(cd "$(dirname "$0")/.." && pwd)

die() { echo "error: $*" >&2; exit 1; }

tag=${1:-}
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: $0 vX.Y.Z"
command -v minisign >/dev/null || die "minisign is not installed"
command -v gh >/dev/null || die "gh is not installed"
[[ -f $key ]] || die "no signing key at $key"

# The app trusts these and nothing else. Checking the new signature against
# them catches signing with the wrong key before anyone downloads it.
mapfile -t trusted < <(sed -n 's/^ *"\(RW[A-Za-z0-9+/=]*\)",.*/\1/p' "$here/src/update.rs")
[[ ${#trusted[@]} -ge 1 ]] || die "could not read the public keys from src/update.rs"

[[ $(gh release view "$tag" -R "$repo" --json isDraft -q .isDraft) == true ]] \
  || die "$tag is not a draft; it is already published, or does not exist"

dir=$here/dist/$tag
rm -rf "$dir" && mkdir -p "$dir"
echo "== downloading the draft into dist/$tag"
gh release download "$tag" -R "$repo" -D "$dir" \
  -p '*.tar.gz' -p '*.zip' -p SHA256SUMS.txt
cd "$dir"

echo "== checking the checksum list"
archives=(*.tar.gz *.zip)
listed=$(awk '{print $2}' SHA256SUMS.txt | sed 's/^\*//' | sort)
[[ $listed == "$(printf '%s\n' "${archives[@]}" | sort)" ]] \
  || die "SHA256SUMS.txt does not list exactly the archives in the release"
for a in "${archives[@]}"; do
  [[ $a == mnm-cartographer-$tag-* ]] || die "$a is not named for $tag"
done
sha256sum -c SHA256SUMS.txt

echo "== checking GitHub's attestation that CI built them from $tag"
for a in "${archives[@]}"; do
  gh attestation verify "$a" -R "$repo" \
    --signer-workflow "$workflow" --source-ref "refs/tags/$tag" >/dev/null \
    || die "$a has no attestation from $workflow at $tag; do not sign it"
  echo "$a: built by release.yml at $tag"
done

echo
echo "Try these exact files before signing:"
for a in "${archives[@]}"; do echo "  $dir/$a"; done
echo
read -rp "Type 'sign' once they work, to sign and publish $tag: " answer
[[ $answer == sign ]] || die "not signed; the draft is unchanged"

# The files must be the ones you just tried, and the draft must still hold
# the same list, or the signature would vouch for something else.
sha256sum -c --quiet SHA256SUMS.txt || die "the files changed while you were testing"
gh release download "$tag" -R "$repo" -p SHA256SUMS.txt -O - | cmp -s - SHA256SUMS.txt \
  || die "the draft's SHA256SUMS.txt changed since it was downloaded"

minisign -S -s "$key" -m SHA256SUMS.txt -t "mnm-cartographer $tag"
ok=
for k in "${trusted[@]}"; do
  minisign -V -q -P "$k" -m SHA256SUMS.txt >/dev/null 2>&1 && ok=1
done
[[ -n $ok ]] || die "signed, but not with a key the app trusts; check $key"

gh release upload "$tag" -R "$repo" SHA256SUMS.txt.minisig --clobber
gh release edit "$tag" -R "$repo" --draft=false --latest
echo "== published $tag: $(gh release view "$tag" -R "$repo" --json url -q .url)"
