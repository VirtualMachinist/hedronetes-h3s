#!/usr/bin/env bash
set -euo pipefail
python3 - <<'PY'
from pathlib import Path
import hashlib,json
root=Path('crates/h3s-apiserver/proto')
for name,source in json.loads((root/'sources.json').read_text())['files'].items():
    actual=hashlib.sha256((root/name).read_bytes()).hexdigest()
    if actual!=source['sha256']:raise SystemExit('Source hash mismatch: '+name)
PY
protoc --version
protoc -I crates/h3s-apiserver/proto --include_imports \
    --descriptor_set_out=crates/h3s-apiserver/proto/kubernetes-v1.34.bin \
    crates/h3s-apiserver/proto/k8s.io/api/{core,rbac,apps,discovery,coordination,authentication,authorization}/v1/generated.proto
sha256sum crates/h3s-apiserver/proto/kubernetes-v1.34.bin
