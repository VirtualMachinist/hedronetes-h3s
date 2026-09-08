//! Embed containerd / youki / CNI / pause bits, and the NixOS persistence contract.

/// Package name, for workspace inventory and later `h3s --version` assembly.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Source of the NixOS module that must replace `/run/systemd/system` units.
pub const NIXOS_MODULE: &str = include_str!("../../../integration/tower/nixos/h3s.nix");

/// DHCP overlay interfaces that must not receive dhcpcd addresses.
pub const DHCP_DENY_INTERFACES: &str = "flannel.* h3s-test0 veth*";

/// Kernel module required for reverse NAT on the Pod bridge.
pub const BRIDGE_MODULE: &str = "br_netfilter";

pub const BRIDGE_SYSCTL: &str = "net.bridge.bridge-nf-call-iptables";

pub fn required_unit_names(role: &str) -> &'static [&'static str] {
    match role {
        "server" => &[
            "h3s-containerd-runtime",
            "h3s-flannel-runtime",
            "h3s-server-runtime",
            "hedronetes-api-firewall",
            "hedronetes-vxlan-firewall",
            "hedronetes-bridge-netfilter",
            "hedronetes-pod-api-firewall",
        ],
        "worker" => &[
            "h3s-containerd-runtime",
            "h3s-flannel-runtime",
            "h3s-worker-runtime",
            "hedronetes-api-firewall",
            "hedronetes-vxlan-firewall",
            "hedronetes-bridge-netfilter",
        ],
        _ => &[],
    }
}

/// True when the shipped NixOS module encodes the bbe7c56 persistence contract.
pub fn nixos_module_encodes_persistence() -> bool {
    NIXOS_MODULE.contains(DHCP_DENY_INTERFACES)
        && NIXOS_MODULE.contains(BRIDGE_MODULE)
        && NIXOS_MODULE.contains(BRIDGE_SYSCTL)
        && NIXOS_MODULE.contains("hedronetes.h3s")
        && required_unit_names("server")
            .iter()
            .all(|name| NIXOS_MODULE.contains(name))
        && required_unit_names("worker")
            .iter()
            .all(|name| NIXOS_MODULE.contains(name))
        && !NIXOS_MODULE.contains("builder")
}
