# Project runtime dependency patches

## youki 0.7.0: exec seccomp preservation

`youki-0.7.0-exec-seccomp.patch` modifies the Apache-2.0 upstream source and is
distributed with the project's `hedronetes-youki-0.7.0-h3s.1` package. The source
archive hash and unchanged upstream Cargo.lock remain pinned in the flake.

The unpatched source build passed its CLI tests and applied seccomp to init,
but the actual containerd CRI exec probe reported `Seccomp: 0`. Its tenant
builder reconstructed the Linux specification without the original seccomp
profile. The patch carries that profile into exec. It also constructs listener
state only for notification profiles, so ordinary filters work without giving
an exec builder ownership of the container's persisted init state. Notification
profiles for exec remain unsupported and fail; this patch does not claim that
capability. M1's runtime-default profile does not use notification actions.

The package runs upstream CLI tests and the tenant-builder tests, including a
regression for preserving both a configured profile and its absence. Actual
NixOS acceptance must additionally verify the init and exec kernel filters,
CPU/memory limits, container logs, and cleanup through the native CRI probe.
Do not waive that probe based on successful compilation or a feature banner.

For an update, check whether the chosen upstream revision includes an equivalent
fix, remove or rebase this patch explicitly, and rerun package and live tests.
Retain the tested source, patch hash, package closure and configuration in the
deployment record. Rollback requires checking the old runtime's behavior; the
unpatched build is not an acceptable fallback for exec with seccomp.

This is a project-carried change. It has not been submitted upstream.

## Flannel 0.28.9: Nodes with absent annotations

The actual upstream ARM64 release panicked in `AcquireLease` when the h3s worker
Node omitted `metadata.annotations`. Flannel deep-copies that Node and writes its
backend keys without first allocating the Go map. An absent map is valid Node
metadata; manually inserting a dummy annotation is not the package fix.

`flannel-0.28.9-nil-annotations.patch` initializes the map on the copied Node.
Existing annotations and informer cache ownership are preserved. The custom
`integration/tower/flannel.nix` package pins upstream source and vendored modules,
installs the patch and Apache license, and wraps the daemon with explicit
iptables/ip6tables and nftables helpers. Flannel's nftables mode still calls the
iptables cleanup manager on startup. No helpers are discovered on the lab host or sent
to protected builders.

The Linux package regression first reverses the patch and requires the original
nil-map panic. It then restores the patch and runs the Kubernetes subnet manager
suite, including absent, empty and populated annotation maps. This test cannot
run as a native macOS test because Flannel's networking package uses Linux APIs.
Real guest execution must still prove startup, Node metadata/conditions, VXLAN,
CNI and nftables behavior; a package build alone does not establish networking.

For updates, check for an equivalent upstream fix, remove or rebase this patch,
and rerun the regression and real network/recovery tests. The old unpatched
release is not a working fallback for an unannotated Node. Retain the exact
source, vendor hash, patch, Go compiler, package and runtime configuration. This
project-carried patch is not published to the upstream Flannel repository.
