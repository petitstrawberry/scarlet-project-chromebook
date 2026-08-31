#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
rpi_host="${SCARLET_RPI_HOST:-192.168.77.2}"
rpi_user="${SCARLET_RPI_USER:-pi}"
identity_file="${SCARLET_RPI_IDENTITY:-$HOME/.ssh/id_rsa}"
remote="$rpi_user@$rpi_host"
ssh_opts=(-i "$identity_file" -o BatchMode=yes -o ConnectTimeout=10)

files=(
  raspi-scarlet-gadget.sh
  raspi-scarlet-gadget.service
  raspi-scarlet-gadget.env
  raspi-scarlet-ncm.sh
  raspi-scarlet-ncm.service
  raspi-scarlet-controller.sh
)

for file in "${files[@]}"; do
  [[ -f "$script_dir/$file" ]] || {
    echo "install-raspi-scarlet: missing file: $script_dir/$file" >&2
    exit 1
  }
done

remote_tmp_dir=$(ssh "${ssh_opts[@]}" "$remote" 'mktemp -d /tmp/scarlet-install.XXXXXX')
restart_gadget="${SCARLET_RPI_RESTART_GADGET:-1}"
case "$restart_gadget" in
  0|1) ;;
  *)
    echo "install-raspi-scarlet: SCARLET_RPI_RESTART_GADGET must be 0 or 1" >&2
    exit 1
    ;;
esac
cleanup() {
  ssh "${ssh_opts[@]}" "$remote" "rm -rf -- '$remote_tmp_dir'" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for file in "${files[@]}"; do
  scp "${ssh_opts[@]}" "$script_dir/$file" "$remote:$remote_tmp_dir/$file"
done

ssh "${ssh_opts[@]}" "$remote" "
  sudo install -d -m 0755 /usr/local/sbin /etc/default
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-gadget.sh' /usr/local/sbin/scarlet-gadget
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-controller.sh' /usr/local/sbin/scarlet-controller
  sudo install -m 0755 '$remote_tmp_dir/raspi-scarlet-ncm.sh' /usr/local/sbin/scarlet-ncm
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.service' /etc/systemd/system/scarlet-gadget.service
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-ncm.service' /etc/systemd/system/scarlet-ncm.service
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.env' /etc/default/scarlet-gadget
  if [[ ! -x /usr/sbin/dnsmasq || ! -x /usr/sbin/nft ]]; then
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends dnsmasq-base nftables
  fi
  sudo systemctl daemon-reload
  sudo systemctl enable scarlet-gadget.service scarlet-ncm.service
  if [[ '$restart_gadget' == 1 ]]; then
    sudo systemctl restart scarlet-gadget.service
  fi
  sudo systemctl restart scarlet-ncm.service
"

if [[ "$restart_gadget" == 1 ]]; then
  echo "Raspberry Pi Scarlet gadget/controller/NCM gateway installed and restarted."
else
  echo "Raspberry Pi Scarlet controller/NCM gateway installed; existing gadget left attached."
fi
