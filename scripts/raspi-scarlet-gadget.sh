#!/usr/bin/env bash
set -euo pipefail

config_env="${SCARLET_CONFIG_FILE:-/etc/default/scarlet-gadget}"
if [[ -r "$config_env" ]]; then
  # The file is installed root-owned by the deployment script.
  . "$config_env"
fi

gadget_name="${SCARLET_GADGET_NAME:-scarlet}"
gadget_root="/sys/kernel/config/usb_gadget/$gadget_name"
runtime_dir="${SCARLET_RUNTIME_DIR:-/run/scarlet}"
current_image="${SCARLET_CURRENT_IMAGE:-$runtime_dir/current.img}"
lock_path="${SCARLET_LOCK_PATH:-/run/lock/scarlet-gadget.lock}"
hid_enabled="${SCARLET_HID_ENABLED:-0}"
config_root="$gadget_root/configs/c.1"
mass_function="$gadget_root/functions/mass_storage.0"
mass_lun="$mass_function/lun.0"
hid_function="$gadget_root/functions/hid.usb0"
hid_device="${SCARLET_HID_DEVICE:-/dev/hidg0}"
hid_descriptor_size=47

die() {
  echo "scarlet-gadget: $*" >&2
  exit 1
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "must run as root"
}

ensure_configfs() {
  command -v mountpoint >/dev/null 2>&1 || die "mountpoint is required"
  if ! mountpoint -q /sys/kernel/config; then
    mount -t configfs none /sys/kernel/config
  fi
  /usr/sbin/modprobe libcomposite 2>/dev/null || true
  /usr/sbin/modprobe usb_f_mass_storage 2>/dev/null || true
}

ensure_hid_module() {
  /usr/sbin/modprobe usb_f_hid 2>/dev/null || die "usb_f_hid kernel module is unavailable"
}

first_udc() {
  local path
  for path in /sys/class/udc/*; do
    [[ -e "$path" ]] || continue
    basename "$path"
    return 0
  done
  return 1
}

write_value() {
  local path=$1 value=$2
  [[ -e "$path" ]] || printf '%s' "$value" > "$path"
  if [[ "$(cat "$path" 2>/dev/null || true)" != "$value" ]]; then
    printf '%s' "$value" > "$path"
  fi
}

ensure_gadget() {
  ensure_configfs
  local strings="$gadget_root/strings/0x409"
  local config="$config_root"
  local config_strings="$config/strings/0x409"
  local serial

  install -d -m 0755 "$runtime_dir" /run/lock
  install -d -m 0755 "$strings" "$config_strings" "$config" "$mass_function"
  serial=$(tr -d '\n' </etc/machine-id 2>/dev/null || true)
  [[ -n "$serial" ]] || serial="scarlet-pi"

  write_value "$gadget_root/idVendor" 0x1d6b
  write_value "$gadget_root/idProduct" 0x0104
  write_value "$gadget_root/bcdDevice" 0x0100
  write_value "$gadget_root/bcdUSB" 0x0200
  write_value "$strings/manufacturer" "Raspberry Pi"
  write_value "$strings/product" "Scarlet USB Storage"
  write_value "$strings/serialnumber" "$serial"
  write_value "$config_strings/configuration" "Mass Storage"
  write_value "$config/MaxPower" 500
  write_value "$mass_function/stall" 0
  write_value "$mass_lun/removable" 1
  write_value "$mass_lun/ro" 0

  if [[ ! -L "$config_root/mass_storage.0" ]]; then
    (cd "$gadget_root" && ln -s functions/mass_storage.0 "$config/mass_storage.0")
  fi
}

hid_report_descriptor() {
  # USB HID boot keyboard: modifiers, reserved byte, six key slots.
  # Keep this as one printf write: configfs treats each write to report_desc
  # as a complete replacement of the descriptor.
  printf '\x05\x01\x09\x06\xa1\x01\x05\x07\x19\xe0\x29\xe7\x15\x00\x25\x01\x75\x01\x95\x08\x81\x02\x95\x01\x75\x08\x81\x01\x95\x05\x75\x01\x05\x08\x19\x01\x29\x05\x91\x02\x95\x01\x75\x03\x91\x01\xc0'
}

ensure_hid() {
  [[ "$hid_enabled" == 1 ]] || return 0
  ensure_hid_module
  local descriptor_size=0
  local recreate=0
  if [[ -r "$hid_function/report_desc" ]]; then
    descriptor_size=$(wc -c <"$hid_function/report_desc" | tr -d ' ')
  fi
  if [[ "$descriptor_size" != "$hid_descriptor_size" ]]; then
    # configfs HID attributes are immutable after the function has been
    # linked once. Recreate a stale/partial function before writing it.
    disable_hid_link
    if [[ -d "$hid_function" ]]; then
      rmdir "$hid_function" || die "failed to recreate HID function"
    fi
    install -d -m 0755 "$hid_function"
    recreate=1
  fi
  write_value "$hid_function/protocol" 1
  write_value "$hid_function/subclass" 1
  write_value "$hid_function/report_length" 8
  if [[ "$recreate" == 1 ]]; then
    hid_report_descriptor > "$hid_function/report_desc"
  fi
  if [[ ! -L "$config_root/hid.usb0" ]]; then
    (cd "$gadget_root" && ln -s functions/hid.usb0 "$config_root/hid.usb0")
  fi
}

disable_hid_link() {
  if [[ -L "$config_root/hid.usb0" ]]; then
    rm -f -- "$config_root/hid.usb0"
  fi
}

remove_hid() {
  disable_hid_link
  if [[ -d "$hid_function" ]]; then
    rmdir "$hid_function" || die "failed to remove disabled HID function"
  fi
}

detach() {
  [[ -e "$gadget_root/UDC" ]] || return 0
  local bound
  bound=$(cat "$gadget_root/UDC" 2>/dev/null || true)
  if [[ -n "$bound" ]]; then
    # configfs unbinds a gadget when UDC receives an empty string.  A
    # zero-byte write is a no-op, so emit the terminating newline explicitly.
    printf '\n' > "$gadget_root/UDC" || return 1
    for _ in {1..20}; do
      [[ -z "$(cat "$gadget_root/UDC" 2>/dev/null || true)" ]] && break
      sleep 0.05
    done
    [[ -z "$(cat "$gadget_root/UDC" 2>/dev/null || true)" ]] || return 1
  fi
  [[ -e "$gadget_root/functions/mass_storage.0/lun.0/file" ]] || return 0
  printf '\n' > "$gadget_root/functions/mass_storage.0/lun.0/file" || return 1
}

attach() {
  local image=${1:?image path is required}
  [[ -f "$image" ]] || die "image does not exist: $image"
  [[ -s "$image" ]] || die "image is empty: $image"
  ensure_gadget
  local udc
  udc=$(first_udc) || die "no USB device controller found"
  detach || die "failed to detach existing USB gadget"
  if [[ "$hid_enabled" == 1 ]]; then
    ensure_hid
  else
    remove_hid
  fi
  printf '%s' "$image" > "$mass_lun/file"
  printf '%s' "$udc" > "$gadget_root/UDC"
}

with_lock() {
  install -d -m 0755 "$(dirname "$lock_path")"
  exec 9>"$lock_path"
  flock -x 9
  "$@"
}

replace_image() {
  local incoming=${1:?incoming image path is required}
  [[ -f "$incoming" ]] || die "incoming image does not exist: $incoming"
  [[ -s "$incoming" ]] || die "incoming image is empty: $incoming"
  ensure_gadget
  detach || die "failed to detach existing USB gadget; refusing to replace the active backing file"
  if [[ "$hid_enabled" == 1 ]]; then
    ensure_hid
  else
    remove_hid
  fi
  install -d -m 0755 "$runtime_dir"
  if [[ "$incoming" != "$current_image" ]]; then
    mv -f -- "$incoming" "$current_image"
  fi
  sync
  attach "$current_image"
}

status() {
  ensure_gadget
  local udc=""
  local hid_configured=0
  [[ -e "$gadget_root/UDC" ]] && udc=$(cat "$gadget_root/UDC" 2>/dev/null || true)
  [[ "$hid_enabled" == 1 && -L "$config_root/hid.usb0" ]] && hid_configured=1
  printf 'gadget=%s\n' "$gadget_name"
  printf 'udc=%s\n' "$udc"
  printf 'image=%s\n' "$current_image"
  if [[ -e "$mass_lun/file" ]]; then
    printf 'lun_file=%s\n' "$(cat "$mass_lun/file" 2>/dev/null || true)"
  fi
  printf 'hid_enabled=%s\n' "$hid_enabled"
  printf 'hid_configured=%s\n' "$hid_configured"
  printf 'hid_device=%s\n' "$( [[ "$hid_enabled" == 1 && -c "$hid_device" ]] && echo "$hid_device" || echo unavailable )"
}

send_key() {
  local key="${1:-}"
  [[ "$hid_enabled" == 1 ]] || die "HID support is disabled"
  [[ -c "$hid_device" ]] || die "HID device is unavailable: $hid_device"

  case "$key" in
    ctrl-l)
      # Left Ctrl + L (HID usage 0x0f), i.e. Depthcharge's altfw shortcut.
      printf '\x01\x00\x0f\x00\x00\x00\x00\x00' > "$hid_device"
      ;;
    enter)
      printf '\x00\x00\x28\x00\x00\x00\x00\x00' > "$hid_device"
      ;;
    ctrl-d)
      printf '\x01\x00\x07\x00\x00\x00\x00\x00' > "$hid_device"
      ;;
    release)
      printf '\x00\x00\x00\x00\x00\x00\x00\x00' > "$hid_device"
      return 0
      ;;
    *)
      die "unknown HID key '$key' (expected ctrl-l, enter, ctrl-d, or release)"
      ;;
  esac

  sleep "${SCARLET_HID_KEY_HOLD_SEC:-0.05}"
  printf '\x00\x00\x00\x00\x00\x00\x00\x00' > "$hid_device"
}

wait_forever() {
  ensure_gadget
  if [[ -f "$current_image" ]]; then
    attach "$current_image"
  fi
  trap detach EXIT INT TERM
  while :; do
    sleep 3600
  done
}

usage() {
  cat >&2 <<'EOF'
usage: scarlet-gadget {attach|detach|replace|status|key|wait} [argument]

Images live in /run/scarlet by default. `replace` detaches the USB LUN,
atomically moves an incoming image to current.img, and re-attaches it.
The deployed configuration is storage-only; keyboard input is sent through
the Chromebook EC over CCD.
EOF
}

require_root
case "${1:-}" in
  attach) with_lock attach "${2:-}" ;;
  detach) with_lock detach ;;
  replace) with_lock replace_image "${2:-}" ;;
  status) with_lock status ;;
  key) with_lock send_key "${2:-}" ;;
  wait) wait_forever ;;
  *) usage; exit 2 ;;
esac
