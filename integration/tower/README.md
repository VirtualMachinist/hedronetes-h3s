# Tower project VM definitions

These guests are isolated under `LIMA_HOME=$H3S_ROOT/lima` (set `H3S_ROOT` to your project root on the host). Never operate the protected Colima `builder` profile or its assets. Do not run global cleanup or change the host's default Lima/Colima configuration.

The approved NixOS pair uses 6+2 vCPU, 12+4 GiB RAM and 80+20 GiB guest capacity. Stop only the expressly disposable Colima `default` before starting the pair. Monitor actual host space and preserve the 25 GiB free-space floor. Both guests use the isolated Lima user-v2 network for cross-node traffic. Initial VZ NAT addresses were assigned but peer ARP failed on Tower; user-v2 is the project-scoped alternative and requires no shared host network changes. No host directories are mounted, and automatic application port forwarding is disabled; project SSH access remains available through Lima.

The bootstrap image is pinned to nixos-lima v0.2.1 and its GitHub release SHA-256. The image's NixOS/tool versions must be inspected and upgraded to the selected pinned execution configuration before acceptance; image startup itself is not an acceptance pass.

Provision on the host with Lima/Homebrew available in PATH and the explicit project `LIMA_HOME`, using `limactl validate` then `limactl start --yes --name=hedronetes-server nixos-server.yaml` and the corresponding worker command. Keep configuration and command logs in the project evidence directory. Never invoke an implicit/default instance.
