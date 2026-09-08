#!/usr/bin/env bash
# Run only inside an approved Hedronetes NixOS guest, as root.
set -euo pipefail
[[ $(id -u) == 0 ]]
[[ -d /home/abdul-qadir.guest/hedronetes-m1 ]]
[[ -e /etc/NIXOS ]]
runtime_tools=${1:?pass the built project runtime-tools store path}
case "$runtime_tools" in /nix/store/*-hedronetes-runtime-tools) ;; *) exit 2 ;; esac
for runtime_binary in containerd containerd-shim-runc-v2 youki; do
  [[ -x "$runtime_tools/bin/$runtime_binary" ]]
done
[[ -x "$runtime_tools/share/hedronetes/cni-bin/bridge" ]]
[[ -x "$runtime_tools/share/hedronetes/cni-bin/host-local" ]]
[[ -x "$runtime_tools/share/hedronetes/cni-bin/loopback" ]]
"$runtime_tools/bin/youki" features | "$runtime_tools/bin/jq" -e '.linux.cgroup.v2 == true and .linux.cgroup.systemd == true and .linux.seccomp.enabled == true'
install -d -m 0700 /var/lib/hedronetes-m1/containerd /var/lib/hedronetes-m1/cni/net.d /var/lib/hedronetes-m1/cni/ipam
umask 077
cat > /var/lib/hedronetes-m1/containerd/config.toml <<EOF
version = 4
root = "/var/lib/hedronetes-m1/containerd/root"
state = "/run/hedronetes-m1/containerd/state"
[grpc]
  address = "/run/hedronetes-m1/containerd/containerd.sock"
  uid = 0
  gid = 0
[plugins."io.containerd.grpc.v1.cri"]
  disable_tcp_service = true
  stream_server_address = "127.0.0.1"
  stream_server_port = "0"
[plugins."io.containerd.cri.v1.images"]
  snapshotter = "overlayfs"
  [plugins."io.containerd.cri.v1.images".pinned_images]
    sandbox = "registry.k8s.io/pause@sha256:f548e0e8e3dc1896ca956272154dde3314e8cc4fde0a57577ee9fa1c63f5baf4"
[plugins."io.containerd.cri.v1.runtime"]
  enable_cdi = false
  unset_seccomp_profile = "runtime/default"
  [plugins."io.containerd.cri.v1.runtime".containerd]
    default_runtime_name = "youki"
    [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.youki]
      runtime_type = "io.containerd.runc.v2"
      [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.youki.options]
        BinaryName = "$runtime_tools/bin/youki"
        SystemdCgroup = true
        Root = "/run/hedronetes-m1/youki"
  [plugins."io.containerd.cri.v1.runtime".cni]
    bin_dirs = ["$runtime_tools/share/hedronetes/cni-bin"]
    conf_dir = "/var/lib/hedronetes-m1/cni/net.d"
    max_conf_num = 1
EOF
cat > /var/lib/hedronetes-m1/cni/net.d/10-h3s-runtime.conflist <<'EOF'
{
  "cniVersion": "1.1.0",
  "name": "h3s-runtime",
  "plugins": [{
    "type": "bridge",
    "bridge": "h3s-test0",
    "isGateway": true,
    "ipMasq": false,
    "ipam": {
      "type": "host-local",
      "dataDir": "/var/lib/hedronetes-m1/cni/ipam",
      "ranges": [[{"subnet": "10.42.2.0/24"}]],
      "routes": [{"dst": "0.0.0.0/0"}]
    }
  }]
}
EOF
cat > /run/systemd/system/h3s-containerd-runtime.service <<EOF
[Unit]
Description=Hedronetes M1 project containerd with youki
After=network.target
[Service]
Type=simple
ExecStart=$runtime_tools/bin/containerd --config /var/lib/hedronetes-m1/containerd/config.toml
Environment=PATH=$runtime_tools/bin:/run/current-system/sw/bin
Restart=on-failure
RestartSec=2
Delegate=yes
KillMode=process
RuntimeDirectory=hedronetes-m1/containerd
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
LimitNOFILE=1048576
TasksMax=infinity
EOF
systemctl daemon-reload
systemctl restart h3s-containerd-runtime.service
systemctl is-active h3s-containerd-runtime.service
