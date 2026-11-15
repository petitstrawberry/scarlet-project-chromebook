#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
default_image="$repo_root/projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img"
image_path="${1:-$default_image}"
. "$(dirname -- "${BASH_SOURCE[0]}")/raspi-ssh.sh"
# Keep the upload on the Pi's disk. /run is a small tmpfs intentionally used
# for the active image, and holding old+new images there can exhaust it before
# the gadget has a chance to switch over.
remote_tmp="${SCARLET_RPI_INCOMING:-/var/tmp/scarlet-incoming.img}"

[[ -f "$image_path" ]] || { echo "image not found: $image_path" >&2; exit 1; }
[[ -s "$image_path" ]] || { echo "image is empty: $image_path" >&2; exit 1; }

remote="${rpi_user}@${rpi_host}"
remote_tmp_dir=$(dirname "$remote_tmp")

if command -v sha256sum >/dev/null 2>&1; then
  image_sha256=$(sha256sum "$image_path" | awk '{print $1}')
else
  image_sha256=$(shasum -a 256 "$image_path" | awk '{print $1}')
fi

echo "Uploading $(du -h "$image_path" | awk '{print $1}') image to $remote..."
ssh "${ssh_opts[@]}" "$remote" "sudo install -d -o $rpi_user -g $rpi_user -m 0755 '$remote_tmp_dir' && sudo rm -f -- '$remote_tmp.part'"
# Full ext2 images contain large zero-filled free regions. SSH compression
# avoids transferring their entire logical size over the controller network.
scp -C "${ssh_opts[@]}" "$image_path" "$remote:$remote_tmp.part"
remote_sha256=$(ssh "${ssh_opts[@]}" "$remote" "sha256sum '$remote_tmp.part'" | awk '{print $1}')
[[ "$remote_sha256" == "$image_sha256" ]] || {
  echo "error: uploaded image checksum mismatch; active image unchanged" >&2
  exit 1
}
echo "Verified uploaded image SHA256: $image_sha256"
ssh "${ssh_opts[@]}" "$remote" \
  "sudo mv -f '$remote_tmp.part' '$remote_tmp' && sudo /usr/local/sbin/scarlet-gadget replace '$remote_tmp'"
ssh "${ssh_opts[@]}" "$remote" "sudo /usr/local/sbin/scarlet-gadget status"
echo "Scarlet image is attached as composite USB storage + CDC-NCM."
