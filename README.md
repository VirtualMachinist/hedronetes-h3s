# Hedronetes (h3s)

<p align="center">
  <img src="assets/hedronetes-seal-dark.jpeg" alt="Hedronetes (h3s) product mark" width="220" />
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-C9A227?style=flat&colorA=111111" alt="Apache-2.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/releases/tag/v0.9.0"><img src="https://img.shields.io/badge/Release-v0.9.0-C9A227?style=flat&colorA=111111" alt="Release v0.9.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/VirtualMachinist/hedronetes-h3s/ci.yml?style=flat&label=CI&colorA=111111&color=C9A227" alt="CI" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-0042DB?style=flat&colorA=111111&logo=rust&logoColor=C9A227" alt="Rust" /></a>
</p>

# Hedronetes (h3s)

**The runtime for agent fleets.**

Hedronetes is the vehicle for your agent fleet. Whether you're running coding agents, trading agents, research agents, or all of the above..h3s is the platform that schedules them, isolates them, restarts them, and keeps their world small enough to reason about.


*This is a Kubernetes-compatible cluster distribution in one Rust binary.*

> k3s, written in Rust, without embedding a Go control plane.

Status: **0.9.0** · License: Apache-2.0 · API target: Kubernetes **v1.34** · Platform: Linux amd64 / arm64

## What this is

Hedronetes distills the idea of k3s into a single Rust binary; `server` / `agent`, SQLite by default, high availability, stock `kubectl` and Helm — and reimplements the control plane as native Rust (Tokio), with a typed kubelet FSM, a pluggable store, **youki** as the default OCI runtime, and **nftables**-first kube-proxy.

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

**v0.9.0 is the first release that actually runs a cluster.** v0.1.0 was the P0 scaffold (`h3s --help`). M1 is done. **Durable high availability with Kubernetes conformance ships with v1.0.0**

A 3-node h3s cluster — native `server` + separate `agent` — runs workloads with stock `kubectl` and Helm. Proven on colima VMs running a mix of NixOS, Debian and Fedora. 


## License

Apache-2.0
