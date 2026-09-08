# Declarative NixOS persistence for the dedicated Hedronetes pair.
# Encodes runtime facts from the bbe7c56 checkpoint so a reboot would not drop
# transient /run/systemd/system units. Does not install onto guests in this
# file; activation is a later step. Do not reference protected lab infrastructure.
{
  lib,
  config,
  pkgs,
  ...
}:
let
  cfg = config.hedronetes.h3s;
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
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = [ "br_netfilter" ];
    boot.kernel.sysctl."net.bridge.bridge-nf-call-iptables" = 1;

    networking.dhcpcd.extraConfig = ''
      denyinterfaces flannel.* h3s-test0 veth*
    '';

    systemd.services.h3s-containerd-runtime = {
      description = "Hedronetes containerd (persistent)";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.h3s-flannel-runtime = {
      description = "Hedronetes Flannel (persistent)";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.h3s-server-runtime = lib.mkIf (cfg.role == "server") {
      description = "Hedronetes h3s API + local agent (persistent)";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.h3s-worker-runtime = lib.mkIf (cfg.role == "worker") {
      description = "Hedronetes h3s worker (persistent)";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.hedronetes-api-firewall = {
      description = "Hedronetes peer API firewall";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.hedronetes-vxlan-firewall = {
      description = "Hedronetes VXLAN/8472 admission";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.hedronetes-bridge-netfilter = {
      description = "Hedronetes br_netfilter + bridge-nf-call-iptables";
      wantedBy = [ "multi-user.target" ];
    };
    systemd.services.hedronetes-pod-api-firewall = lib.mkIf (cfg.role == "server") {
      description = "Hedronetes Pod CIDR to API (server only)";
      wantedBy = [ "multi-user.target" ];
    };
  };
}
