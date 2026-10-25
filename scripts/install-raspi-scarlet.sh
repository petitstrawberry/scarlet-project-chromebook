#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$(dirname -- "${BASH_SOURCE[0]}")/raspi-ssh.sh"
remote="$rpi_user@$rpi_host"

files=(
  raspi-scarlet-gadget.sh
  raspi-scarlet-gadget.service
  raspi-scarlet-gadget.env
  raspi-scarlet-ncm.sh
  raspi-scarlet-ncm.service
  raspi-scarlet-controller.sh
  raspi-scarlet-ccd.py
  raspi-scarlet-ccd.service
)

for file in "${files[@]}"; do
  [[ -f "$script_dir/$file" ]] || {
    echo "install-raspi-scarlet: missing file: $script_dir/$file" >&2
    exit 1
  }
done

# Preserve an installed Pi configuration unless explicitly asked to replace it.
replace_config="${SCARLET_RPI_REPLACE_CONFIG:-0}"
[[ "$replace_config" == 0 || "$replace_config" == 1 ]] || { echo 'SCARLET_RPI_REPLACE_CONFIG must be 0 or 1' >&2; exit 2; }
restart_gadget="${SCARLET_RPI_RESTART_GADGET:-1}"
case "$restart_gadget" in
  0|1) ;;
  *)
    echo "install-raspi-scarlet: SCARLET_RPI_RESTART_GADGET must be 0 or 1" >&2
    exit 1
    ;;
esac
remote_tmp_dir=$(ssh "${ssh_opts[@]}" "$remote" 'mktemp -d /tmp/scarlet-install.XXXXXX')
cleanup() {
  ssh "${ssh_opts[@]}" "$remote" "rm -rf -- '$remote_tmp_dir'" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for file in "${files[@]}"; do
  scp "${ssh_opts[@]}" "$script_dir/$file" "$remote:$remote_tmp_dir/$file"
done

ssh "${ssh_opts[@]}" "$remote" "
  set -euo pipefail
  sudo install -d -m 0755 /usr/local/sbin /etc/default
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-gadget.sh' /usr/local/sbin/scarlet-gadget
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-controller.sh' /usr/local/sbin/scarlet-controller
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-ccd.py' /usr/local/sbin/scarlet-ccd
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-ccd.service' /etc/systemd/system/scarlet-ccd.service
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-ncm.sh' /usr/local/sbin/scarlet-ncm
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.service' /etc/systemd/system/scarlet-gadget.service
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-ncm.service' /etc/systemd/system/scarlet-ncm.service
  if [[ '$replace_config' == 1 || ! -f /etc/default/scarlet-gadget ]]; then
    if [[ -f /etc/default/scarlet-gadget ]]; then
      sudo cp -a /etc/default/scarlet-gadget /etc/default/scarlet-gadget.previous
    fi
    sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.env' /etc/default/scarlet-gadget
  fi
  if [[ ! -x /usr/sbin/dnsmasq || ! -x /usr/sbin/nft ]] || ! /usr/bin/python3 -c 'import usb.core' 2>/dev/null || [[ ! -x /usr/sbin/avahi-daemon ]]; then
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends dnsmasq-base nftables python3-usb avahi-daemon
  fi
  sudo systemctl daemon-reload
  sudo systemctl enable scarlet-gadget.service scarlet-ncm.service scarlet-ccd.service
  sudo systemctl enable --now avahi-daemon.service
  sudo systemctl restart scarlet-ccd.service
  if [[ '$restart_gadget' == 1 ]]; then
    sudo systemctl restart scarlet-gadget.service
  fi
  sudo systemctl restart scarlet-ncm.service
"

if [[ "$restart_gadget" == 1 ]]; then
  echo "Raspberry Pi Scarlet gadget/controller/CCD/NCM gateway installed and restarted."
else
  echo "Raspberry Pi Scarlet controller/CCD/NCM gateway installed; existing gadget left attached."
fi
