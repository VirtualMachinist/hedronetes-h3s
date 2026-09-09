# Hedronetes (h3s)

<p align="center">
  <img src="assets/hedronetes-seal-dark.jpeg" alt="Hedronetes (h3s) product mark" width="220" />
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-C9A227?style=flat&colorA=111111" alt="Apache-2.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/releases/tag/v1.0.0"><img src="https://img.shields.io/badge/Release-v1.0.0-C9A227?style=flat&colorA=111111" alt="Release v1.0.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/VirtualMachinist/hedronetes-h3s/ci.yml?style=flat&label=CI&colorA=111111&color=C9A227" alt="CI" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-0042DB?style=flat&colorA=111111&logo=rust&logoColor=C9A227" alt="Rust" /></a>
</p>

**A Kubernetes-compatible cluster distribution in one Rust binary.**

> k3s, written in Rust, without embedding a Go control plane.

Status: **1.0.0 · M1** · License: Apache-2.0 · API target: Kubernetes **v1.34** · Platform: Linux amd64 / arm64

Repo: [VirtualMachinist/hedronetes-h3s](https://github.com/VirtualMachinist/hedronetes-h3s)

## What this is

Hedronetes copies k3s’s *product* shape — one binary, `server` / `agent`, SQLite by default, HA when you need it (M2), stock `kubectl` and Helm — and reimplements the control plane as native Rust (Tokio), with a typed kubelet FSM, a pluggable store, **youki** as the default OCI runtime, and **nftables**-first kube-proxy.

It does **not** embed upstream Go Kubernetes.

## Docs

- Full product specification: [`SPEC.md`](./SPEC.md). The spec covers adjacent agentic planes (cluster / retrieve / record) as a product constraint, not a shipped feature.

## Binary

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

**v1.0.0 is the first release that actually runs a cluster.** v0.1.0 was the P0 scaffold (`h3s --help`). M1 is done.

A two-node h3s cluster — native `server` + separate `agent` — runs workloads with stock `kubectl` and Helm. Proven on NixOS (full six-component integration) and as a portable Linux install on Debian 13 and Fedora 43 (no Nix at runtime). Overlay is Flannel VXLAN; ClusterIP traffic is native nftables; ClusterFirst DNS is CoreDNS. Runtime is containerd + youki.

It is **not** a certified Kubernetes distribution and **not** HA. Those are M2. The API target remains Kubernetes **v1.34** for the documented subset.

See [security](docs/security-foundation.md), [API](docs/api-foundation.md), [worker bootstrap](docs/worker-bootstrap.md), [service networking](docs/service-networking.md), [CRI](docs/cri-runtime.md), and [Tower integration](integration/tower/README.md).

## License

Apache-2.0
