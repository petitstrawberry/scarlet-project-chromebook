#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project_dir="$repo_root/projects/aarch64-coachz-limine"
profile_flag="--release"
profile_dir="release"
staging_dir="$project_dir/.scarlet/staging"
stamp_dir="$project_dir/.scarlet/image-stamps"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug)
      profile_flag=""
      profile_dir="debug"
      shift
      ;;
    *)
      echo "usage: $0 [--debug]" >&2
      exit 2
      ;;
  esac
done

# cargo-scarlet leaves this generated tree behind when an image build fails.
# Starting from it again can collide with bundle-created symlinks.
if [[ -d "$staging_dir" ]]; then
  rm -rf -- "$staging_dir"
fi

# Repackage every image from this build while retaining Cargo's compilation
# cache. Dependency revisions come from the source workspaces' Cargo.lock files.
for image_name in initramfs rootfs boot disk; do
  stamp_path="$stamp_dir/$image_name.stamp"
  if [[ -e "$stamp_path" ]]; then
    rm -- "$stamp_path"
  fi
done

if [[ -n "$profile_flag" ]]; then
  cargo scarlet image --project "$project_dir" "$profile_flag"
else
  cargo scarlet image --project "$project_dir"
fi

# Refuse to hand off an image assembled from stale cached binaries.  This
# checks the kernel in the EFI image and the selected Scarlet graphics
# applications listed below in the ext2 rootfs.
kernel_elf="$project_dir/bsp/target/aarch64-unknown-none-elf/$profile_dir/scarlet"
esp_image="$project_dir/.scarlet/images/esp-aarch64-coachz.img"
rootfs_image="$project_dir/.scarlet/images/rootfs-aarch64-coachz-full.ext2"

kernel_hash=$(shasum -a 256 "$kernel_elf" | awk '{print $1}')
image_kernel_hash=$(mcopy -i "$esp_image" ::/boot/kernel - 2>/dev/null | shasum -a 256 | awk '{print $1}')
if [[ "$image_kernel_hash" != "$kernel_hash" ]]; then
  echo "CoachZ image verification failed: EFI kernel is stale" >&2
  exit 1
fi

for binary in \
  sgfx-probe terminal ui-demo ui-benchmark settings video-player \
  sws scarlet-shell clock files notepad task-manager ui-sgfx-showcase \
  sgfx-cube sgfx-texture sgfx-showcase boxcraft
do
  staged_binary="$staging_dir/rootfs/bin/$binary"
  staged_hash=$(shasum -a 256 "$staged_binary" | awk '{print $1}')
  image_hash=$(debugfs -R "cat /bin/$binary" "$rootfs_image" 2>/dev/null | shasum -a 256 | awk '{print $1}')
  if [[ "$image_hash" != "$staged_hash" ]]; then
    echo "CoachZ image verification failed: rootfs $binary is stale" >&2
    exit 1
  fi
done

echo "CoachZ image verification passed: kernel and SGFX binaries match this build" >&2
