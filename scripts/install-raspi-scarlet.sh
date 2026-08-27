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
  raspi-scarlet-controller.sh
)

for file in "${files[@]}"; do
  [[ -f "$script_dir/$file" ]] || {
    echo "install-raspi-scarlet: missing file: $script_dir/$file" >&2
    exit 1
  }
done

remote_tmp_dir=$(ssh "${ssh_opts[@]}" "$remote" 'mktemp -d /tmp/scarlet-install.XXXXXX')
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
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.service' /etc/systemd/system/scarlet-gadget.service
  sudo install -m 0644 '$remote_tmp_dir/raspi-scarlet-gadget.env' /etc/default/scarlet-gadget
  sudo systemctl daemon-reload
  sudo systemctl enable scarlet-gadget.service
  sudo systemctl restart scarlet-gadget.service
"

echo "Raspberry Pi Scarlet gadget/controller installed and restarted."
