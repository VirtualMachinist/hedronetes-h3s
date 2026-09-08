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
