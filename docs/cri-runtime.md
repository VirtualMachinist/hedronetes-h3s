# Native CRI runtime integration

`h3s-cri` generates the full Kubernetes v1.34 CRI v1 protocol from the unmodified
upstream proto and exposes typed tonic runtime/image clients. `proto/SOURCE.json`
records its origin and SHA-256. Tonic/tonic-prost 0.14.6 supports the workspace's
Rust 1.88 minimum. The pinned `protoc-bin-vendored` 3.2.0 build dependency carries
host-specific protoc binaries through Cargo; generation performs no separate
network download and requires no ambient protoc installation.

`Cri::connect` accepts an absolute Unix socket path, optionally prefixed with
`unix://`. It rejects relative paths, parent traversal and network URLs. There
is no Docker protocol, network connection, proxy or fallback. Version negotiation
requires CRI v1. Connects have five-second limits; runtime RPCs have 30 seconds,
image RPCs 180 seconds. Local timeouts and gRPC deadline headers both apply;
messages are capped at 16 MiB and each channel has sixteen in-flight requests
and a bounded queue. Runtime status errors are reduced to their code rather
than echoing arbitrary runtime error details into operator logs.

The multicall binary exposes read-only inspection:

```sh
sudo h3s runtime-info --container-runtime-endpoint \
  unix:///run/hedronetes-m1/containerd/containerd.sock
```

It prints actual runtime/version/condition fields. This does not mark the node
Ready or claim the kubelet can yet reconcile Pods.

## Pinned NixOS runtime tools

The flake supplies `containerd`, `youki`, and `runtime-tools` outputs. Containerd 2.3.5 and CNI plugins 1.9.1 use hash-verified upstream static ARM64
release archives. Youki 0.7.0 is built from its hash-pinned upstream source with
its unchanged Cargo.lock and explicit `v2,systemd,seccomp,cgroupsv2_devices`
features. It links the pinned Nix libseccomp/ELF/zlib libraries. The upstream
musl release launched containers but reported no cgroup support and no seccomp;
the direct fixture caught its missing filter. That artifact is not accepted as
the runtime. Both package installation and guest configuration now reject a
youki binary unless its OCI feature report enables v2, systemd and seccomp.
These versions are newer than the pinned Nixpkgs entries. Install checks launch
each runtime and the CNI bridge VERSION operation on Linux.
No global tool or protected lab configuration is changed.

The runtime-tools environment puts runtime/system utilities in `bin`, but keeps
CNI plugin executables at `share/hedronetes/cni-bin`. This avoids the name
collision between CNI's `bridge` plugin and iproute2's `bridge` command. The
configuration uses explicit paths rather than relying on an ambient PATH.

`integration/tower/configure-containerd.sh` is a project-only NixOS guest
bootstrap, invoked as root with the built runtime-tools store path. It creates
only the named project containerd/CNI configuration and runtime service. The
socket and runtime directory are root-only. The default OCI handler is youki
through containerd's runc-v2 shim, using systemd cgroups and an explicit youki
binary. It retains seccomp and sets the default profile to runtime/default.
The pause image is pinned by manifest digest. Streaming endpoints bind loopback.

The initial CNI fixture allocates 10.42.2.0/24 on bridge `h3s-test0`, with IPAM
state under `/var/lib/hedronetes-m1/cni/ipam`. This is a local runtime fixture;
node CIDR allocation, cross-node routes, Service/DNS and final CNI integration
remain required. Do not apply the same fixed subnet to both nodes.

The service uses project paths under `/var/lib/hedronetes-m1` and
`/run/hedronetes-m1`. Containerd uses its native version 4 configuration; streaming fields belong to
the `io.containerd.grpc.v1.cri` plugin. The unit is transient under `/run/systemd/system`; reboot
persistence and native h3s supervision/NixOS module integration remain unfinished.
`KillMode=process` allows containerd shims to survive a daemon restart. Stopping
the daemon alone is not workload cleanup. Remove only identified project
sandboxes through CRI before teardown; never remove arbitrary runtime objects
or flush host routes/firewall rules. Preserve prior package roots and data for
rollback; changing the runtime binary alone does not prove data compatibility.

## Verification

Four tests run a labelled CRI protocol fixture over a real Unix socket to verify
version negotiation, deadline propagation/enforcement, refusal of network and
missing endpoints, unimplemented RPC behavior and sanitized errors. They are
not evidence of real containers.

The `runtime-probe` example instead calls a real containerd daemon through this
client. It requires an explicit socket, private log root and digest-pinned image.
It creates its own uniquely labelled sandbox/container, expects the configured
CNI address, runs with a non-root UID, dropped capabilities, no-new-privileges,
read-only root filesystem and runtime-default seccomp, verifies exec and log
output, and reads the actual task PID's kernel cgroup files to verify memory/CPU
limits and init-process seccomp. It cleans only its invocation's labelled resources. Image cache
and sanitized log files remain. A failed RPC may have committed; cleanup uses
labelled runtime queries to find those resources as well.

```sh
cargo test -p h3s-cri --locked
cargo build -p h3s-cri --example runtime-probe --locked
sudo /path/to/runtime-probe unix:///run/hedronetes-m1/containerd/containerd.sock \
  /var/lib/hedronetes-m1/runtime-probes \
  docker.io/library/busybox@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0
```

The direct CRI fixture is a step toward the real kubelet runtime, not a substitute
for scheduled Kubernetes workloads or M1 acceptance. Installed results and
failures belong in the project's checkpoint evidence before claiming success.

Sources: [Kubernetes CRI v1.34](https://github.com/kubernetes/cri-api/blob/v0.34.0/pkg/apis/runtime/v1/api.proto),
[containerd configuration](https://github.com/containerd/containerd/blob/v2.3.5/docs/cri/config.md),
[containerd 2.3.5](https://github.com/containerd/containerd/releases/tag/v2.3.5),
[youki 0.7.0](https://github.com/youki-dev/youki/releases/tag/v0.7.0).
