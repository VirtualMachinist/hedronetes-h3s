//! NICKEL G4b: h3s hears Nickel as generated Rust, never as a VM. No crate in
//! this workspace links `nickel-lang-core`, and the `h3s` binary has no
//! `apply` applet: `:6443` receives frozen Kubernetes JSON/YAML only.
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[test]
fn no_nickel_vm_in_the_dependency_graph() {
    let root = workspace_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("nickel"),
        "Cargo.lock must not resolve any nickel crate"
    );
    let mut manifests = vec![root.join("Cargo.toml")];
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    assert!(
        manifests.len() > 1,
        "expected crate manifests under crates/"
    );
    for manifest in manifests {
        let text = fs::read_to_string(&manifest).unwrap();
        assert!(
            !text.contains("nickel"),
            "{} must not depend on a nickel crate",
            manifest.display()
        );
    }
}

#[test]
fn h3s_binary_has_no_apply_applet() {
    let main = fs::read_to_string(workspace_root().join("crates/h3s/src/main.rs")).unwrap();
    for needle in ["Apply(", "\"apply\"", "--ncl"] {
        assert!(
            !main.contains(needle),
            "h3s must not grow an apply applet ({needle}); kubectl and Facet are the writers"
        );
    }
}
