use h3s_packaging::{
    nixos_module_encodes_persistence, required_unit_names, BRIDGE_MODULE, BRIDGE_SYSCTL,
    DHCP_DENY_INTERFACES, NIXOS_MODULE,
};

#[test]
fn shipped_nixos_module_encodes_persistence_contract() {
    assert!(
        nixos_module_encodes_persistence(),
        "integration/tower/nixos/h3s.nix must keep DHCP deny, br_netfilter, and unit names"
    );
    assert!(NIXOS_MODULE.contains(DHCP_DENY_INTERFACES));
    assert!(NIXOS_MODULE.contains(BRIDGE_MODULE));
    assert!(NIXOS_MODULE.contains(&format!("\"{BRIDGE_SYSCTL}\" = 1")));
    for name in required_unit_names("server") {
        assert!(NIXOS_MODULE.contains(name), "missing server unit {name}");
    }
    for name in required_unit_names("worker") {
        assert!(NIXOS_MODULE.contains(name), "missing worker unit {name}");
    }
    assert!(!NIXOS_MODULE.contains("builder"));
}
