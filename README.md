# Hedronetes (h3s)

**A Kubernetes-compatible cluster distribution in one Rust binary.**

> k3s, written in Rust, without embedding a Go control plane.

Status: **0.1.0-draft** · License: Apache-2.0 · API target: Kubernetes **v1.34** · Platform: Linux amd64 / arm64

## What this is

Hedronetes copies k3s’s *product* shape — one binary, `server` / `agent`, SQLite by default, HA when you need it, stock `kubectl` and Helm — and reimplements the control plane as native Rust (Tokio), with a typed kubelet FSM, a pluggable store, **youki** as the default OCI runtime, and **nftables**-first kube-proxy.

It does **not** embed upstream Go Kubernetes.

## Docs

- Full product specification: [`SPEC.md`](./SPEC.md)

## Binary (planned)

```text
h3s server   # control plane + datastore + supervisor (+ embedded agent)
h3s agent    # kubelet + kube-proxy + CNI + runtime + tunnel client
```

## Status

Founding seed only. Implementation has not started. Spec is normative for design review.

## License

Apache-2.0
