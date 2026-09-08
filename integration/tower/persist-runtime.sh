#!/usr/bin/env bash
# Persist live transient units via /var/lib + tmpfiles.d (NixOS /etc/systemd/system is a static symlink).
# Do not restart h3s/flannel/containerd. Do not reboot.
set -euo pipefail
DEST=/var/lib/hedronetes-m1/systemd
mkdir -p "$DEST" "$DEST/dhcpcd.service.d"
for u in /run/systemd/system/h3s-*.service /run/systemd/system/hedronetes-*.service; do
  [ -f "$u" ] || continue
  cp -a "$u" "$DEST/$(basename "$u")"
done
if [ -d /run/systemd/system/dhcpcd.service.d ]; then
  cp -a /run/systemd/system/dhcpcd.service.d/. "$DEST/dhcpcd.service.d/" || true
fi
{
  echo '# Hedronetes: restore transient units into /run on boot'
  for u in "$DEST"/*.service; do
    [ -f "$u" ] || continue
    echo "C /run/systemd/system/$(basename "$u") 0644 root root - $u"
  done
  if [ -d "$DEST/dhcpcd.service.d" ]; then
    echo "d /run/systemd/system/dhcpcd.service.d 0755 root root -"
    for u in "$DEST/dhcpcd.service.d"/*; do
      [ -f "$u" ] || continue
      echo "C /run/systemd/system/dhcpcd.service.d/$(basename "$u") 0644 root root - $u"
    done
  fi
} > /etc/tmpfiles.d/90-hedronetes-m1.conf
printf 'net.bridge.bridge-nf-call-iptables = 1\n' > /etc/sysctl.d/90-hedronetes-m1.conf
printf 'br_netfilter\n' > /etc/modules-load.d/90-hedronetes-m1.conf
sysctl -w net.bridge.bridge-nf-call-iptables=1 >/dev/null
modprobe br_netfilter || true
systemd-tmpfiles --create /etc/tmpfiles.d/90-hedronetes-m1.conf >/dev/null
echo PERSIST_OK
ls "$DEST"/*.service | wc -l
cat /etc/tmpfiles.d/90-hedronetes-m1.conf | wc -l
