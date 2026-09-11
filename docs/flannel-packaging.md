# Flannel package and integration checkpoint

`nix build .#flannel --option builders "" --option max-jobs 2 --option cores 2`
builds the project Flannel package on the supported ARM64 Linux flake target.
It uses upstream 0.28.9 source, the small nil-annotation repair and a fixed Go
module hash in `integration/tower/flannel.nix`. Go is a dependency build tool;
Flannel is an external networking component, not an embedded Go control plane.

The upstream release panics when a freshly registered Node has no annotations.
The project patch initializes the copied Node's map before publishing Flannel's
own backend data. Package tests reverse that patch and require the original
panic, restore it, then test the subnet manager with absent, empty and populated
maps. See `integration/tower/patches/README.md` for the repair and update policy.

The wrapper at `bin/flannel` supplies exact iptables/ip6tables and nftables helper
paths. Even with `EnableNFTables: true`, Flannel runs its iptables cleanup manager
before initializing nftables. The underlying `bin/.flannel-wrapped` executable
is built with CGO disabled. Retain its hash, Go build information and actual ELF
linkage inspection before using it as a portable artifact. Debian/Fedora runs
must install and verify their own declared helpers without Nix; the Nix wrapper
itself is not a portable installation.

## Tower lab configuration

The worker daemon runs as the root `h3s-flannel-runtime.service`, using its own
node certificate in a mode-0600 kubeconfig under the private project network
directory. The certificate is derived inside the guest from the native agent's
identity. No admin credential, join token or secret is placed in the Nix store.

The configuration selects `10.42.0.0/16`, IPv4, nftables and VXLAN VNI 1/UDP 8472.
`NODE_NAME=hedronetes-worker`, explicit kubeconfig, interface/public IP and
`--subnet-file=/run/hedronetes/flannel/subnet.env` avoid host discovery.
The health listener binds only `127.0.0.1:19091`. The daemon consumes the actual
worker `10.42.2.0/24` Node allocation, publishes its own VXLAN metadata and
NetworkUnavailable condition, and creates its VXLAN device and Flannel nftables
tables. The live probe verifies startup from an unannotated Node, daemon restart
and native h3s heartbeat preservation.

This unit and configuration are an integration checkpoint. The runtime still
uses its earlier local bridge CNI configuration. Server-local-agent integration,
Flannel on the server, CNI plugin migration, the native nftables Service proxy,
CoreDNS, cross-node traffic and persistent declarative lifecycle remain required.
No cross-node or portable-network acceptance follows from this daemon probe.

## Recovery and removal

Keep the Flannel subnet file, Node CIDR reservation and runtime/CNI allocation
consistent. Do not reassign the subnet when restarting or replacing a binary.
The private kubeconfig must be refreshed if the agent identity changes; complete
certificate renewal and native lifecycle supervision remain required work.

For this checkpoint, stop the exact project Flannel unit before replacement.
Retain the tested package with the project GC root. The original unpatched
release is retained only as failure evidence; it is not a working rollback for
an unannotated Node. Avoid removing network state while Pods use it.

When tearing down an empty project guest, remove only the project unit, private
Flannel configuration/credential files and its GC root, then the verified owned
`flannel.1` device and `flannel-ipv4`/`flannel-ipv6` nftables tables. Remove Flannel
Node annotations and its NetworkUnavailable condition through authenticated API
updates with fresh UID/resourceVersion guards. Never flush an entire ruleset,
remove other interfaces or reclaim a Node CIDR automatically. Final operator
setup/teardown must automate and verify this scope before M1 completion.
