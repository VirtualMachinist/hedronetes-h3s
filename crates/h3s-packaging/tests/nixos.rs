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
    assert!(
        NIXOS_MODULE.contains("h3s server"),
        "module must encode h3s server ExecStart, not an empty unit stub"
    );
    assert!(
        NIXOS_MODULE.contains("h3s agent"),
        "module must encode h3s agent ExecStart"
    );
    assert!(NIXOS_MODULE.contains("--cluster-dns"));
    assert!(NIXOS_MODULE.contains("--container-runtime-endpoint"));
    assert!(
        NIXOS_MODULE.contains("/bin/containerd"),
        "module must start project containerd"
    );
    assert!(
        NIXOS_MODULE.contains("/bin/flannel"),
        "module must start patched flannel"
    );
}

#[test]
fn portable_install_layout_is_explicit() {
    let paths = h3s_packaging::portable_install_paths();
    assert!(paths.contains(&"bin/h3s"));
    assert!(paths.contains(&"share/hedronetes/cni-bin"));
    assert_eq!(h3s_packaging::persistent_unit_dir(), "/etc/systemd/system");
}
