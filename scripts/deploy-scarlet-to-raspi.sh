#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
default_image="$repo_root/projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img"
image_path="${1:-$default_image}"
rpi_host="${SCARLET_RPI_HOST:-192.168.77.2}"
rpi_user="${SCARLET_RPI_USER:-pi}"
remote_tmp="${SCARLET_RPI_INCOMING:-/run/scarlet/incoming.img}"
identity_file="${SCARLET_RPI_IDENTITY:-$HOME/.ssh/id_rsa}"

[[ -f "$image_path" ]] || { echo "image not found: $image_path" >&2; exit 1; }
[[ -s "$image_path" ]] || { echo "image is empty: $image_path" >&2; exit 1; }

ssh_opts=(-i "$identity_file" -o BatchMode=yes -o ConnectTimeout=10)
remote="${rpi_user}@${rpi_host}"

echo "Uploading $(du -h "$image_path" | awk '{print $1}') image to $remote..."
ssh "${ssh_opts[@]}" "$remote" "sudo install -d -o $rpi_user -g $rpi_user -m 0755 /run/scarlet && rm -f '$remote_tmp.part'"
scp "${ssh_opts[@]}" "$image_path" "$remote:$remote_tmp.part"
ssh "${ssh_opts[@]}" "$remote" \
  "sudo mv -f '$remote_tmp.part' '$remote_tmp' && sudo /usr/local/sbin/scarlet-gadget replace '$remote_tmp'"
ssh "${ssh_opts[@]}" "$remote" "sudo /usr/local/sbin/scarlet-gadget status"
echo "Scarlet image is attached as USB mass storage."
