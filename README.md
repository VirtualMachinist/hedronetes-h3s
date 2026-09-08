# Hedronetes (h3s)

<p align="center">
  <img src="assets/hedronetes-seal-dark.jpeg" alt="Hedronetes (h3s) product mark" width="220" />
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-C9A227?style=flat&colorA=111111" alt="Apache-2.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/releases/tag/v0.1.0"><img src="https://img.shields.io/badge/Release-v0.1.0-C9A227?style=flat&colorA=111111" alt="Release v0.1.0" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-0042DB?style=flat&colorA=111111&logo=rust&logoColor=C9A227" alt="Rust" /></a>
</p>

**A Kubernetes-compatible cluster distribution in one Rust binary.**

> k3s, written in Rust, without embedding a Go control plane.

Status: **0.1.0-draft** · License: Apache-2.0 · API target: Kubernetes **v1.34** · Platform: Linux amd64 / arm64

Repo: [VirtualMachinist/hedronetes-h3s](https://github.com/VirtualMachinist/hedronetes-h3s)

## What this is

Hedronetes copies k3s’s *product* shape — one binary, `server` / `agent`, SQLite by default, HA when you need it, stock `kubectl` and Helm — and reimplements the control plane as native Rust (Tokio), with a typed kubelet FSM, a pluggable store, **youki** as the default OCI runtime, and **nftables**-first kube-proxy.

It does **not** embed upstream Go Kubernetes.

## Docs

- Full product specification: [`SPEC.md`](./SPEC.md). The spec covers adjacent agentic planes (cluster / retrieve / record) as a product constraint, not a shipped feature.

## Binary (P0 stub)

```text
h3s server   # control plane + datastore + supervisor (+ embedded agent)
h3s agent    # kubelet + kube-proxy + CNI + runtime + tunnel client
```

```bash
cargo run -p h3s -- --help
cargo run -p h3s -- server --help
cargo run -p h3s -- agent --help
```

## Status

P0 workspace: empty crates compile. `h3s --help` / `h3s server --help` / `h3s agent --help` are clap multicall stubs. API server and kubelet are not implemented. Spec is normative for design review.

## License

Apache-2.0
