#!/usr/bin/env bash
set -euo pipefail

project_dir="${SCARLET_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
image="$project_dir/.scarlet/images/esp-aarch64-coachz.img"

if [[ -z "${SCARLET_COACHZ_LIMINE_EFI:-}" ]]; then
  echo "SCARLET_COACHZ_LIMINE_EFI is unset; run cargo scarlet inside 'nix develop'." >&2
  exit 1
fi
if [[ ! -f "$SCARLET_COACHZ_LIMINE_EFI" ]]; then
  echo "patched CoachZ Limine EFI not found: $SCARLET_COACHZ_LIMINE_EFI" >&2
  exit 1
fi
if [[ ! -f "$image" ]]; then
  echo "Scarlet image was not produced: $image" >&2
  exit 1
fi

verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/coachz-limine-verify.XXXXXX")"
trap 'rm -r -- "$verify_dir"' EXIT

current_efi="$verify_dir/current-BOOTAA64.EFI"
if mcopy -i "$image" ::/EFI/BOOT/BOOTAA64.EFI "$current_efi" 2>/dev/null \
  && cmp -s "$SCARLET_COACHZ_LIMINE_EFI" "$current_efi"
then
  echo "CoachZ Limine EFI is already current: $image"
else
  # The Limine plugin creates a directly mountable FAT32 ESP. Replace only
  # its loader; kernel, initramfs, and configuration remain plugin-owned.
  mcopy -o -i "$image" "$SCARLET_COACHZ_LIMINE_EFI" ::/EFI/BOOT/BOOTAA64.EFI
  echo "Injected patched Limine EFI: $SCARLET_COACHZ_LIMINE_EFI"
fi

verified_efi="$verify_dir/verified-BOOTAA64.EFI"
mcopy -i "$image" ::/EFI/BOOT/BOOTAA64.EFI "$verified_efi"
cmp "$SCARLET_COACHZ_LIMINE_EFI" "$verified_efi"

echo "CoachZ ESP ready: $image"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$image"
else
  shasum -a 256 "$image"
fi
