#!/usr/bin/env bash
# Configure the project CRI with Flannel CNI on either approved NixOS node.
# Caller must stop scheduling/agents and verify the API has no live workloads.
set -euo pipefail
[[ $(id -u) == 0 ]]
H3S_ROOT="${H3S_ROOT:-/var/lib/hedronetes}"
H3S_RUN="${H3S_RUN:-/run/hedronetes}"
[[ -d "$H3S_ROOT" ]]
[[ -e /etc/NIXOS ]]
runtime_tools=${1:?pass the built project runtime-tools store path}
flannel_cni=${2:?pass the built project Flannel CNI store path}
backup_dir=${3:?pass a new private project backup directory}
case "$flannel_cni" in /nix/store/*-hedronetes-flannel-cni-1.9.1-flannel3) ;; *) exit 2 ;; esac
case "$backup_dir" in "$H3S_ROOT"/network/*) ;; *) exit 2 ;; esac
[[ -x "$flannel_cni/bin/flannel" ]]
[[ ! -e "$backup_dir" ]]
case "$runtime_tools" in /nix/store/*-hedronetes-runtime-tools) ;; *) exit 2 ;; esac
for runtime_binary in containerd containerd-shim-runc-v2 youki; do
  [[ -x "$runtime_tools/bin/$runtime_binary" ]]
done
[[ -x "$runtime_tools/share/hedronetes/cni-bin/bridge" ]]
[[ -x "$runtime_tools/share/hedronetes/cni-bin/host-local" ]]
[[ -x "$runtime_tools/share/hedronetes/cni-bin/loopback" ]]
"$runtime_tools/bin/youki" features | "$runtime_tools/bin/jq" -e '.linux.cgroup.v2 == true and .linux.cgroup.systemd == true'
"$runtime_tools/bin/youki" --version | "$runtime_tools/bin/jq" -R -s -e 'test("(?m)^libseccomp: [0-9]+[.][0-9]+[.][0-9]+$")'
# Replacing CNI under live sandboxes would strand their cleanup state.
runtime_socket="$H3S_RUN/containerd/containerd.sock"
if [[ -S "$runtime_socket" ]]; then
  [[ -z $("$runtime_tools/bin/ctr" --address "$runtime_socket" --namespace k8s.io tasks list --quiet) ]]
  [[ -z $("$runtime_tools/bin/ctr" --address "$runtime_socket" --namespace k8s.io containers list --quiet) ]]
fi
install -d -m 0700 "$backup_dir"
for previous in "$H3S_ROOT/containerd/config.toml" "$H3S_ROOT/cni/net.d/10-h3s-runtime.conflist" /run/systemd/system/h3s-containerd-runtime.service; do
  if [[ -e "$previous" ]]; then
    cp -p "$previous" "$backup_dir/$(basename "$previous")"
  fi
done
install -d -m 0700 "$H3S_ROOT/cni/flannel" "$H3S_ROOT/containerd" "$H3S_ROOT/cni/net.d" "$H3S_ROOT/cni/ipam"
umask 077
cat > "$H3S_ROOT/containerd/config.toml" <<EOF
version = 4
root = "$H3S_ROOT/containerd/root"
state = "$H3S_RUN/containerd/state"
imports = []
[plugins."io.containerd.server.v1.grpc"]
  address = "$H3S_RUN/containerd/containerd.sock"
  uid = 0
  gid = 0
[plugins."io.containerd.server.v1.ttrpc"]
  address = "$H3S_RUN/containerd/containerd.sock.ttrpc"
  uid = 0
  gid = 0
[plugins."io.containerd.server.v1.grpc-tcp"]
  address = ""
[plugins."io.containerd.internal.v1.opt"]
  path = "$H3S_ROOT/containerd/opt"
[plugins."io.containerd.nri.v1.nri"]
  disable = true
[plugins."io.containerd.grpc.v1.cri"]
  disable_tcp_service = true
  stream_server_address = "127.0.0.1"
  stream_server_port = "0"
[plugins."io.containerd.cri.v1.images"]
  snapshotter = "overlayfs"
  [plugins."io.containerd.cri.v1.images".pinned_images]
    sandbox = "registry.k8s.io/pause@sha256:f548e0e8e3dc1896ca956272154dde3314e8cc4fde0a57577ee9fa1c63f5baf4"
[plugins."io.containerd.cri.v1.runtime"]
  cdi_spec_dirs = []
  netns_mounts_under_state_dir = true
  unset_seccomp_profile = "runtime/default"
  [plugins."io.containerd.cri.v1.runtime".containerd]
    default_runtime_name = "youki"
    [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.youki]
      runtime_type = "io.containerd.runc.v2"
      [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.youki.options]
        BinaryName = "$runtime_tools/bin/youki"
        SystemdCgroup = true
        Root = "$H3S_RUN/youki"
  [plugins."io.containerd.cri.v1.runtime".cni]
    bin_dirs = ["$flannel_cni/bin", "$runtime_tools/share/hedronetes/cni-bin"]
    conf_dir = "$H3S_ROOT/cni/net.d"
    max_conf_num = 1
EOF
cat > "$H3S_ROOT/cni/net.d/10-h3s-runtime.conflist" <<EOF
{
  "cniVersion": "1.1.0",
  "name": "h3s-runtime",
  "plugins": [{
    "type": "flannel",
    "subnetFile": "$H3S_RUN/flannel/subnet.env",
    "dataDir": "$H3S_ROOT/cni/flannel",
    "ipam": {
      "type": "host-local",
      "dataDir": "$H3S_ROOT/cni/ipam",
      "routes": [{"dst": "0.0.0.0/0"}]
    },
    "delegate": {
      "type": "bridge",
      "bridge": "h3s-test0",
      "isGateway": true,
      "hairpinMode": true,
      "ipMasq": false
    }
  }]
}
EOF
# Reuse the existing project bridge so migration never creates overlapping
# connected subnet routes. Flannel supplies each node's CIDR and VXLAN MTU.
if "$runtime_tools/bin/ip" link show h3s-test0 >/dev/null 2>&1; then
  "$runtime_tools/bin/ip" link set h3s-test0 mtu 1450
fi
cat > /run/systemd/system/h3s-containerd-runtime.service <<EOF
[Unit]
Description=Hedronetes project containerd with youki
After=network.target
[Service]
Type=simple
ExecStart=$runtime_tools/bin/containerd --config $H3S_ROOT/containerd/config.toml
Environment=PATH=$runtime_tools/bin:/run/current-system/sw/bin
Restart=on-failure
RestartSec=2
Delegate=yes
KillMode=process
RuntimeDirectory=hedronetes/containerd
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
LimitNOFILE=1048576
TasksMax=infinity
EOF
systemctl daemon-reload
systemctl restart h3s-containerd-runtime.service
systemctl is-active h3s-containerd-runtime.service
# A running daemon can still ignore an obsolete socket setting. Require the
# configured private socket and a successful RPC before reporting provisioned.
for ((runtime_attempt=0; runtime_attempt<30; runtime_attempt++)); do
  if [[ -S "$runtime_socket" ]] && "$runtime_tools/bin/ctr" --address "$runtime_socket" --timeout 1s version; then
    [[ $(stat -c '%u:%g' "$runtime_socket") == 0:0 ]]
    [[ $(stat -c '%a' "$(dirname "$runtime_socket")") == 700 ]]
    exit 0
  fi
  sleep 0.5
done
echo 'Project containerd socket failed readiness verification' >&2
exit 1
