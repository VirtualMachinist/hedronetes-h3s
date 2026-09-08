# Flannel CNI integration

The ARM64 flake exports `flannel-cni`, pinned to upstream
`v1.9.1-flannel3`. The release archive is named `v1.9.1`, contains
`flannel-arm64`, and is installed as `bin/flannel` for CNI lookup. The derivation
verifies the archive hash, installs the upstream Apache-2.0 license, and checks
that the executable advertises CNI 1.1.0. The original source for the release is
[flannel-io/cni-plugin](https://github.com/flannel-io/cni-plugin/tree/v1.9.1-flannel3).

`integration/tower/configure-flannel-containerd.sh` is a scoped, temporary
operator setup for either dedicated project NixOS guest. It takes the pinned
runtime-tools package, Flannel CNI package, and a new project-owned backup
directory. Stop project scheduling/agents and verify the API has no workloads
before invocation. The script also refuses existing CRI tasks or containers,
backs up previous runtime/CNI/unit files, and configures the real private CRI
socket with the existing youki security settings. It is not the final native
runtime supervisor or declarative NixOS module.

Containerd supplies both plugin directories. Flannel CNI reads
`/run/hedronetes-m1/flannel/subnet.env`, delegates to the bridge plugin, and
derives IPAM ranges and MTU from the daemon's assigned subnet. Its private
scratch records live under `/var/lib/hedronetes-m1/cni/flannel`; host-local
allocations remain under `/var/lib/hedronetes-m1/cni/ipam`. Explicit default
routes and hairpin mode support Pod traffic; Flannel owns masquerading.
The existing project bridge `h3s-test0` is reused to avoid overlapping routes.
The fixture uses a 1500-byte underlay and 1450-byte VXLAN MTU. This is an IPv4
VXLAN fixture, not an assertion of all CNI backends or IPv6 support.

Keep the Flannel daemon's credentials at private runtime paths outside Nix.
Retain package GC roots and the captured backup. Roll back only after draining
and removing project Pods through their current CNI, stopping the project
agents, and verifying empty CRI/IPAM state; restore matching configuration and
packages together. Do not restore old CNI configuration under running Pods.
The daemon package is described in [Flannel packaging](flannel-packaging.md).
Actual cross-node workload tests, recovery and cleanup are required in addition
to the derivation's version check.
