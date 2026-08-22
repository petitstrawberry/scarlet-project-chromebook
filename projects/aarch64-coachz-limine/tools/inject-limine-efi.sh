#!/usr/bin/env bash
set -euo pipefail

project_dir="${SCARLET_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
repo_root="$(cd "$project_dir/../.." && pwd)"
standalone_esp="${SCARLET_COACHZ_ESP_IMAGE:-$project_dir/.scarlet/images/esp-aarch64-coachz.img}"
combined_disk="${SCARLET_COACHZ_DISK_IMAGE:-}"
if [[ -z "$combined_disk" && -z "${SCARLET_COACHZ_ESP_IMAGE:-}" ]]; then
  combined_disk="$project_dir/.scarlet/images/scarlet-aarch64-coachz-full.img"
fi
uboot_build_dir="${UBOOT_BUILD_DIR:-$repo_root/.cache/u-boot/build-coachz}"
control_dtb="${UBOOT_COACHZ_CONTROL_DTB:-$uboot_build_dir/dts/upstream/src/arm64/qcom/sc7180-trogdor-coachz-r3.dtb}"
handoff_dtb="${SCARLET_COACHZ_OS_DTB:-$uboot_build_dir/scarlet-os-handoff.dtb}"
handoff_builder="$project_dir/tools/build-os-handoff-dtb.sh"
esp_dtb_path='::/dtb/qcom/sc7180-google-coachz.dtb'

if [[ -z "${SCARLET_COACHZ_LIMINE_EFI:-}" ]]; then
  echo "SCARLET_COACHZ_LIMINE_EFI is unset; run cargo scarlet inside 'nix develop'." >&2
  exit 1
fi
if [[ ! -f "$SCARLET_COACHZ_LIMINE_EFI" ]]; then
  echo "patched CoachZ Limine EFI not found: $SCARLET_COACHZ_LIMINE_EFI" >&2
  exit 1
fi
if [[ ! -f "$standalone_esp" && ( -z "$combined_disk" || ! -f "$combined_disk" ) ]]; then
  echo "Scarlet images were not produced: $standalone_esp${combined_disk:+ or $combined_disk}" >&2
  exit 1
fi
if [[ ! -x "$handoff_builder" ]]; then
  echo "CoachZ OS handoff DTB builder is not executable: $handoff_builder" >&2
  exit 1
fi

# U-Boot's fdtfile logic asks the ESP for qcom/sc7180-google-coachz.dtb.
# Regenerate the separate handoff DTB on every image injection so it is always
# derived from the exact HS-only DTB built with the U-Boot payload.
"$handoff_builder" --source "$control_dtb" --output "$handoff_dtb"

verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/coachz-limine-verify.XXXXXX")"
trap 'rm -r -- "$verify_dir"' EXIT

ensure_fat_directory() {
  local fat_image="$1"
  local path="$2"
  if ! mdir -i "$fat_image" "$path" >/dev/null 2>&1; then
    mmd -i "$fat_image" "$path"
  fi
}

read_le_u32() {
  local image="$1"
  local offset="$2"
  local value
  value="$(od -An -v -tu4 -j "$offset" -N 4 "$image" | tr -d '[:space:]')"
  [[ -n "$value" ]] || {
    echo "error: failed to read GPT field at byte $offset from $image" >&2
    return 1
  }
  printf '%s\n' "$value"
}

read_le_u64() {
  local image="$1"
  local offset="$2"
  local low high
  low="$(read_le_u32 "$image" "$offset")"
  high="$(read_le_u32 "$image" "$((offset + 4))")"
  printf '%s\n' "$((low + high * 4294967296))"
}

combined_esp_mtools_image() {
  local image="$1"
  local signature entry_lba entry_count entry_size entry_offset first_lba last_lba image_bytes

  signature="$(od -An -v -tx1 -j 512 -N 8 "$image" | tr -d '[:space:]')"
  [[ "$signature" == '4546492050415254' ]] || {
    echo "error: combined image lacks a primary GPT header: $image" >&2
    return 1
  }

  entry_lba="$(read_le_u64 "$image" $((512 + 72)))"
  entry_count="$(read_le_u32 "$image" $((512 + 80)))"
  entry_size="$(read_le_u32 "$image" $((512 + 84)))"
  [[ "$entry_count" -ge 1 && "$entry_size" -ge 128 ]] || {
    echo "error: combined image has an invalid GPT entry table: $image" >&2
    return 1
  }

  entry_offset="$((entry_lba * 512))"
  # GPT stores the EFI System Partition type GUID in little-endian byte order.
  [[ "$(od -An -v -tx1 -j "$entry_offset" -N 16 "$image" | tr -d '[:space:]')" == '28732ac11ff8d211ba4b00a0c93ec93b' ]] || {
    echo "error: combined image partition 1 is not an EFI System Partition: $image" >&2
    return 1
  }

  first_lba="$(read_le_u64 "$image" $((entry_offset + 32)))"
  last_lba="$(read_le_u64 "$image" $((entry_offset + 40)))"
  image_bytes="$(wc -c < "$image" | tr -d '[:space:]')"
  [[ "$first_lba" -gt 1 && "$last_lba" -ge "$first_lba" && "$((first_lba * 512))" -lt "$image_bytes" ]] || {
    echo "error: combined image partition 1 bounds are invalid: $image" >&2
    return 1
  }

  # mtools addresses a filesystem embedded in an image as IMAGE@@BYTE_OFFSET.
  printf '%s@@%s\n' "$image" "$((first_lba * 512))"
}

inject_and_verify_esp() {
  local fat_image="$1"
  local label="$2"
  local safe_label current_efi verified_efi verified_dtb

  safe_label="${label//[^[:alnum:]]/_}"
  current_efi="$verify_dir/${safe_label}-current-BOOTAA64.EFI"
  if mcopy -i "$fat_image" ::/EFI/BOOT/BOOTAA64.EFI "$current_efi" 2>/dev/null \
    && cmp -s "$SCARLET_COACHZ_LIMINE_EFI" "$current_efi"
  then
    echo "CoachZ Limine EFI is already current: $label"
  else
    # The Limine plugin creates a directly mountable FAT32 ESP. Replace only
    # its loader; kernel, initramfs, and configuration remain plugin-owned.
    mcopy -o -i "$fat_image" "$SCARLET_COACHZ_LIMINE_EFI" ::/EFI/BOOT/BOOTAA64.EFI
    echo "Injected patched Limine EFI into: $label"
  fi

  verified_efi="$verify_dir/${safe_label}-verified-BOOTAA64.EFI"
  mcopy -i "$fat_image" ::/EFI/BOOT/BOOTAA64.EFI "$verified_efi"
  cmp "$SCARLET_COACHZ_LIMINE_EFI" "$verified_efi"

  ensure_fat_directory "$fat_image" '::/dtb'
  ensure_fat_directory "$fat_image" '::/dtb/qcom'
  mcopy -o -i "$fat_image" "$handoff_dtb" "$esp_dtb_path"

  verified_dtb="$verify_dir/${safe_label}-sc7180-google-coachz.dtb"
  mcopy -i "$fat_image" "$esp_dtb_path" "$verified_dtb"
  cmp "$handoff_dtb" "$verified_dtb"

  [[ "$(fdtget -t s "$verified_dtb" /soc@0/phy@88e8000 status)" == 'okay' ]]
  [[ "$(fdtget -t x "$verified_dtb" /soc@0/usb@a6f8800/usb@a600000 phys)" == "$(fdtget -t x "$handoff_dtb" /soc@0/usb@a6f8800/usb@a600000 phys)" ]]
  [[ "$(fdtget -t s "$verified_dtb" /soc@0/usb@a6f8800/usb@a600000 phy-names)" == 'usb2-phy usb3-phy' ]]
  [[ "$(fdtget -t s "$verified_dtb" /soc@0/usb@a6f8800/usb@a600000 maximum-speed)" == 'super-speed' ]]
  [[ "$(fdtget -t s "$verified_dtb" /soc@0/geniqup@ac0000/i2c@a84000 status)" == 'okay' ]]
  [[ "$(fdtget -t s "$verified_dtb" /soc@0/geniqup@ac0000/i2c@a84000/trackpad@15 compatible)" == 'elan,ekth3000' ]]
  echo "Installed CoachZ Scarlet OS handoff DTB in $label: $esp_dtb_path"
}

if [[ -f "$standalone_esp" ]]; then
  inject_and_verify_esp "$standalone_esp" "standalone ESP $standalone_esp"
fi

if [[ -n "$combined_disk" && -f "$combined_disk" ]]; then
  combined_esp="$(combined_esp_mtools_image "$combined_disk")"
  inject_and_verify_esp "$combined_esp" "combined GPT partition 1 $combined_disk"
elif [[ -n "$combined_disk" ]]; then
  echo "CoachZ combined disk is not present; standalone ESP injection completed: $combined_disk"
fi

if [[ -f "$standalone_esp" ]]; then
  echo "CoachZ standalone ESP ready: $standalone_esp"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$standalone_esp"
  else
    shasum -a 256 "$standalone_esp"
  fi
fi

if [[ -n "$combined_disk" && -f "$combined_disk" ]]; then
  echo "CoachZ combined GPT disk ready: $combined_disk"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$combined_disk"
  else
    shasum -a 256 "$combined_disk"
  fi
fi
