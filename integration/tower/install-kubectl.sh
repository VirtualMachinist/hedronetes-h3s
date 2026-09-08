#!/usr/bin/env bash
set -euo pipefail
# Run inside the project ARM64 Linux guest. The destination is explicit.
: "${1:?usage: install-kubectl.sh /absolute/project/bin/kubectl}"
case "$1" in /*) ;; *) echo 'destination must be absolute' >&2; exit 1;; esac
if [[ -e "$1" ]]; then echo 'destination already exists; refusing replacement' >&2; exit 1; fi
[[ "$(uname -s)/$(uname -m)" == Linux/aarch64 ]]
mkdir -p "$(dirname "$1")"
artifact=$(mktemp "${1}.XXXXXX")
trap 'rm -f "$artifact"' EXIT
curl -fSL https://dl.k8s.io/release/v1.34.11/bin/linux/arm64/kubectl -o "$artifact"
python3 - "$artifact" <<'PY'
import hashlib,sys
with open(sys.argv[1],'rb') as source:
    actual=hashlib.file_digest(source,'sha256').hexdigest()
if actual!='5b045a4712674c88a56fd98eef4285689738b7fbe8735e1b9ee3509521af5cb4':
    raise SystemExit('kubectl artifact hash mismatch')
PY
chmod 755 "$artifact"
"$artifact" version --client -o json
# Link atomically without clobbering an executable that appeared meanwhile.
ln "$artifact" "$1"
