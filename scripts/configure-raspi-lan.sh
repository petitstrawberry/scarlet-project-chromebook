#!/usr/bin/env bash
# Stage DHCP for the next Ethernet connection without dropping this SSH session.
set -euo pipefail
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/raspi-ssh.sh"
if [[ $# -ne 0 ]]; then
  echo 'usage: configure-raspi-lan.sh (settings: SCARLET_RPI_HOST/USER/IDENTITY)' >&2
  exit 2
fi
ssh "${ssh_opts[@]}" "$remote" 'sudo -n bash -s' <<'REMOTE'
set -euo pipefail
profile='Scarlet LAN DHCP'
command -v nmcli >/dev/null
[[ -d /sys/class/net/eth0 ]] || { echo 'eth0 is unavailable' >&2; exit 1; }
if nmcli -g connection.uuid connection show "$profile" >/dev/null 2>&1; then
  [[ "$(nmcli -g connection.type connection show "$profile")" == 802-3-ethernet &&
     "$(nmcli -g connection.interface-name connection show "$profile")" == eth0 ]] || {
    echo 'Existing Scarlet LAN DHCP profile does not belong to eth0' >&2
    exit 1
  }
else
  nmcli connection add type ethernet ifname eth0 con-name "$profile" ipv4.method auto ipv6.method auto
fi
nmcli connection modify "$profile" connection.autoconnect yes \
  connection.autoconnect-priority 100 connection.autoconnect-retries 0 \
  connection.mdns 2 ipv4.method auto ipv4.addresses '' ipv4.gateway '' \
  ipv4.dns '' ipv4.ignore-auto-dns no ipv4.ignore-auto-routes no \
  ipv4.never-default no ipv4.may-fail no ipv4.dhcp-timeout 30
# Keep the current connection up, but do not pick the old static profile again.
legacy='Scarlet direct Ethernet'
if nmcli -g connection.uuid connection show "$legacy" >/dev/null 2>&1; then
  if [[ "$(nmcli -g ipv4.method connection show "$legacy")" == manual &&
        "$(nmcli -g ipv4.addresses connection show "$legacy")" == 192.168.77.2/24 ]]; then
    nmcli connection modify "$legacy" connection.autoconnect no
  fi
fi
systemctl enable --now avahi-daemon.service
nmcli -f connection.id,connection.autoconnect,connection.mdns,ipv4.method connection show "$profile"
printf '\nReconnect eth0 to the router, then connect using %s.local.\n' "$(hostname)"
REMOTE
