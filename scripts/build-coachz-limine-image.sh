#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project_dir="$repo_root/projects/aarch64-coachz-limine"
image="$project_dir/.scarlet/images/esp-aarch64-coachz.img"
profile_flag="--release"

if [[ "${1:-}" == "--debug" ]]; then
  profile_flag=""
  shift
fi
if [[ $# -ne 0 ]]; then
  echo "usage: $0 [--debug]" >&2
  exit 2
fi

if [[ -z "${SCARLET_COACHZ_LIMINE_EFI:-}" ]]; then
  echo "SCARLET_COACHZ_LIMINE_EFI is unset; run this command inside 'nix develop'." >&2
  exit 1
fi
if [[ ! -f "$SCARLET_COACHZ_LIMINE_EFI" ]]; then
  echo "patched CoachZ Limine EFI not found: $SCARLET_COACHZ_LIMINE_EFI" >&2
  exit 1
fi

if [[ -n "$profile_flag" ]]; then
  cargo scarlet image --project "$project_dir" "$profile_flag"
else
  cargo scarlet image --project "$project_dir"
fi

if [[ ! -f "$image" ]]; then
  echo "Scarlet image was not produced: $image" >&2
  exit 1
fi

# The Scarlet Limine plugin creates a directly mountable FAT32 ESP. Replace
# only the EFI loader; kernel, initramfs, and configuration remain plugin-owned.
mcopy -o -i "$image" "$SCARLET_COACHZ_LIMINE_EFI" ::/EFI/BOOT/BOOTAA64.EFI

verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/coachz-limine-verify.XXXXXX")"
trap 'rm -r -- "$verify_dir"' EXIT
mcopy -i "$image" ::/EFI/BOOT/BOOTAA64.EFI "$verify_dir/BOOTAA64.EFI"
cmp "$SCARLET_COACHZ_LIMINE_EFI" "$verify_dir/BOOTAA64.EFI"

echo "Injected patched Limine EFI: $SCARLET_COACHZ_LIMINE_EFI"
echo "CoachZ ESP ready: $image"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$image"
else
  shasum -a 256 "$image"
fi
