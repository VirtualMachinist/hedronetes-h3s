# Kubernetes Protobuf descriptors

These Apache-2.0 schemas are copied unchanged from the Kubernetes `api` and `apimachinery` repositories at `v0.34.11`. Original copyright/license headers remain in every file. `sources.json` records each upstream URL and SHA-256. They define wire types, not a Go control plane or runtime dependency.

The checked-in `kubernetes-v1.34.bin` descriptor was generated with `libprotoc 34.1` from the pinned Nix development shell. Its SHA-256 is `a8612b0e54343845648f4ed0f4dbb9ba2b6b9ba163d2644038622366b6f09e7d`. Runtime decoding uses prost-reflect and the descriptor; ordinary Cargo builds do not require protoc or network schema downloads.

Regenerate from the repository root using `nix develop --option builders "" --command bash crates/h3s-apiserver/proto/regenerate.sh`. Review descriptor/hash changes and re-run wire/client integration tests whenever schemas or the generator change.

The decoder verifies the `k8s\0` prefix, runtime.Unknown envelope, and supported GVK before decoding the raw resource. Kubernetes-specific Time, Quantity, IntOrString, raw JSON, bytes, and integer representations are converted before the same k8s-openapi validation used for JSON requests. The descriptor includes several upcoming M1 groups, but the API only accepts kinds registered in its implemented resource table.
