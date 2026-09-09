# Declarative NixOS persistence for the dedicated Hedronetes pair.
# Replaces /run/systemd/system transient units. Credentials stay under
# runtimeRoot, never in the Nix store. Do not reference protected lab
# infrastructure.
{
  lib,
  config,
  pkgs,
  ...
}:
let
  cfg = config.hedronetes.h3s;
  toolsBin = "${cfg.runtimeTools}/bin";
  kubeconfig = "${cfg.runtimeRoot}/runtime/server/admin.kubeconfig";
  cri = "unix:///run/hedronetes-m1/containerd/containerd.sock";
  commonPath = "${toolsBin}:/run/current-system/sw/bin";
in
{
  options.hedronetes.h3s = {
    enable = lib.mkEnableOption "Hedronetes h3s persistence (firewall, DHCP, kernel, units)";
    role = lib.mkOption {
      type = lib.types.enum [ "server" "worker" ];
      description = "Guest role; server runs the API, worker runs CRI/Flannel/proxy.";
    };
    runtimeRoot = lib.mkOption {
      type = lib.types.path;
      default = "/home/abdul-qadir.guest/hedronetes-m1";
      description = "Guest project root; credentials stay here, never in the Nix store.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      description = "h3s package whose bin/h3s is ExecStart.";
    };
    runtimeTools = lib.mkOption {
      type = lib.types.package;
      description = "Project runtime-tools (containerd, nft, iptables).";
    };
    flannel = lib.mkOption {
      type = lib.types.package;
      description = "Patched Flannel package.";
    };
    nodeName = lib.mkOption {
      type = lib.types.str;
      description = "Kubernetes Node name.";
    };
    nodeIp = lib.mkOption {
      type = lib.types.str;
      description = "Node InternalIP on the Lima user-v2 network.";
    };
    peerIp = lib.mkOption {
      type = lib.types.str;
      description = "Peer node IP (worker for server, server for worker).";
    };
    serverUrl = lib.mkOption {
      type = lib.types.str;
      default = "https://192.168.104.1:6443";
      description = "API URL used by the worker agent.";
    };
    clusterDns = lib.mkOption {
      type = lib.types.str;
      default = "10.43.0.10";
    };
    clusterDomain = lib.mkOption {
      type = lib.types.str;
      default = "cluster.local";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = [ "br_netfilter" ];
    boot.kernel.sysctl."net.bridge.bridge-nf-call-iptables" = 1;

    networking.dhcpcd.extraConfig = ''
      denyinterfaces flannel.* h3s-test0 veth*
    '';

    systemd.services.h3s-containerd-runtime = {
      description = "Hedronetes containerd (persistent)";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.runtimeTools}/bin/containerd --config /var/lib/hedronetes-m1/containerd/config.toml";
        Environment = "PATH=${commonPath}";
        Restart = "on-failure";
        RestartSec = 2;
        Delegate = "yes";
        KillMode = "process";
        RuntimeDirectory = "hedronetes-m1/containerd";
        RuntimeDirectoryMode = "0700";
        RuntimeDirectoryPreserve = "yes";
        LimitNOFILE = 1048576;
        TasksMax = "infinity";
      };
    };

    systemd.services.h3s-flannel-runtime = {
      description = "Hedronetes Flannel (persistent)";
      after = [ "network.target" ]
        ++ lib.optionals (cfg.role == "server") [ "h3s-server-runtime.service" ]
        ++ lib.optionals (cfg.role == "worker") [ "h3s-worker-runtime.service" ];
      wants = lib.optionals (cfg.role == "server") [ "h3s-server-runtime.service" ]
        ++ lib.optionals (cfg.role == "worker") [ "h3s-worker-runtime.service" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        NODE_NAME = cfg.nodeName;
        PATH = commonPath;
      };
      serviceConfig = {
        Type = "notify";
        RuntimeDirectory = "hedronetes-m1/flannel";
        RuntimeDirectoryMode = "0700";
        ExecStart = "${cfg.flannel}/bin/flannel --kube-subnet-mgr --kubeconfig-file=${cfg.runtimeRoot}/network/flannel/node.kubeconfig --net-config-path=${cfg.runtimeRoot}/network/flannel/net-conf.json --subnet-file=/run/hedronetes-m1/flannel/subnet.env --iface=${cfg.nodeIp} --public-ip=${cfg.nodeIp} --ip-masq --healthz-ip=127.0.0.1 --healthz-port=19091";
        Restart = "on-failure";
        RestartSec = 3;
        TimeoutStartSec = 90;
        UMask = "0077";
      };
    };

    systemd.services.h3s-server-runtime = lib.mkIf (cfg.role == "server") {
      description = "Hedronetes h3s API + local agent (persistent)";
      after = [ "network.target" "h3s-containerd-runtime.service" ];
      wants = [ "h3s-containerd-runtime.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/h3s server --data-dir ${cfg.runtimeRoot}/runtime --write-kubeconfig ${kubeconfig} --tls-san ${cfg.nodeIp} --node-name ${cfg.nodeName} --node-ip ${cfg.nodeIp} --container-runtime-endpoint ${cri} --service-proxy-nft ${toolsBin}/nft --cluster-dns ${cfg.clusterDns} --cluster-domain ${cfg.clusterDomain}";
        Restart = "on-failure";
        RestartSec = 2;
        UMask = "0077";
      };
    };

    systemd.services.h3s-worker-runtime = lib.mkIf (cfg.role == "worker") {
      description = "Hedronetes h3s worker (persistent)";
      after = [ "network.target" "h3s-containerd-runtime.service" ];
      wants = [ "h3s-containerd-runtime.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/h3s agent --server ${cfg.serverUrl} --server-ca-file ${cfg.runtimeRoot}/config/ca.crt --node-name ${cfg.nodeName} --node-ip ${cfg.nodeIp} --data-dir ${cfg.runtimeRoot}/runtime --container-runtime-endpoint ${cri} --service-proxy-nft ${toolsBin}/nft --cluster-dns ${cfg.clusterDns} --cluster-domain ${cfg.clusterDomain}";
        Restart = "on-failure";
        RestartSec = 2;
      };
    };

    systemd.services.hedronetes-api-firewall = {
      description = "Hedronetes peer API firewall";
      after = [ "firewall.service" ];
      requires = [ "firewall.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.iptables}/bin/iptables -I nixos-fw 3 -i enp0s1 -s ${cfg.peerIp}/32 -d ${cfg.nodeIp}/32 -p tcp --dport 6443 -m comment --comment hedronetes-m1-api -j nixos-fw-accept";
        ExecStop = "-${pkgs.iptables}/bin/iptables -D nixos-fw -i enp0s1 -s ${cfg.peerIp}/32 -d ${cfg.nodeIp}/32 -p tcp --dport 6443 -m comment --comment hedronetes-m1-api -j nixos-fw-accept";
      };
    };

    systemd.services.hedronetes-vxlan-firewall = {
      description = "Hedronetes VXLAN/8472 admission";
      after = [ "firewall.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.iptables}/bin/iptables -I nixos-fw 3 -i enp0s1 -s ${cfg.peerIp}/32 -p udp --dport 8472 -m comment --comment hedronetes-m1-vxlan -j nixos-fw-accept";
        ExecStop = "-${pkgs.iptables}/bin/iptables -D nixos-fw -i enp0s1 -s ${cfg.peerIp}/32 -p udp --dport 8472 -m comment --comment hedronetes-m1-vxlan -j nixos-fw-accept";
      };
    };

    systemd.services.hedronetes-bridge-netfilter = {
      description = "Hedronetes br_netfilter + bridge-nf-call-iptables";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.kmod}/bin/modprobe br_netfilter";
        ExecStartPost = "${pkgs.procps}/bin/sysctl -w net.bridge.bridge-nf-call-iptables=1";
      };
    };

    systemd.services.hedronetes-pod-api-firewall = lib.mkIf (cfg.role == "server") {
      description = "Hedronetes Pod CIDR to API (server only)";
      after = [ "firewall.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.iptables}/bin/iptables -I nixos-fw 3 -s 10.42.0.0/16 -d ${cfg.nodeIp}/32 -p tcp --dport 6443 -m comment --comment hedronetes-m1-pod-api -j nixos-fw-accept";
        ExecStop = "-${pkgs.iptables}/bin/iptables -D nixos-fw -s 10.42.0.0/16 -d ${cfg.nodeIp}/32 -p tcp --dport 6443 -m comment --comment hedronetes-m1-pod-api -j nixos-fw-accept";
      };
    };
  };
}
