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
config_root="$gadget_root/configs/c.1"
mass_function="$gadget_root/functions/mass_storage.0"
mass_lun="$mass_function/lun.0"
ncm_ifname="${SCARLET_NCM_IFNAME:-usb0}"
ncm_function="$gadget_root/functions/ncm.$ncm_ifname"
hid_function="$gadget_root/functions/hid.usb0"
hid_device="${SCARLET_HID_DEVICE:-/dev/hidg0}"
hid_descriptor_size=47

# Keep the USB composition declarative. `mass_storage` is mandatory because
# the Scarlet boot command line uses /dev/usbblk0p2; additional functions can
# be enabled by adding their names to SCARLET_GADGET_FUNCTIONS.
if [[ "${SCARLET_GADGET_FUNCTIONS+x}" == x ]]; then
  function_spec="$SCARLET_GADGET_FUNCTIONS"
else
  function_spec="mass_storage,ncm"
  [[ "${SCARLET_HID_ENABLED:-0}" == 1 ]] && function_spec+=",hid"
fi

declare -a gadget_functions=()

die() {
  echo "scarlet-gadget: $*" >&2
  exit 1
}

parse_function_spec() {
  local csv="$1"
  local function_name
  local seen=,

  [[ -n "$csv" ]] || die "SCARLET_GADGET_FUNCTIONS must not be empty"
  IFS=',' read -r -a gadget_functions <<< "$csv"
  [[ "${#gadget_functions[@]}" -gt 0 ]] || die "no USB gadget functions were requested"

  for function_name in "${gadget_functions[@]}"; do
    [[ -n "$function_name" ]] || die "SCARLET_GADGET_FUNCTIONS contains an empty function"
    case "$function_name" in
      mass_storage|ncm|hid) ;;
      *) die "unsupported USB gadget function '$function_name' (expected mass_storage, ncm, or hid)" ;;
    esac
    [[ "$function_name" != *[[:space:]]* ]] || die "USB gadget function names may not contain whitespace"
    [[ "$seen" != *",$function_name,"* ]] || die "duplicate USB gadget function '$function_name'"
    seen+="$function_name,"
  done

  function_requested mass_storage || die "mass_storage must remain enabled for Scarlet boot"
}

function_requested() {
  local requested="$1"
  local function_name
  for function_name in "${gadget_functions[@]}"; do
    [[ "$function_name" == "$requested" ]] && return 0
  done
  return 1
}

parse_function_spec "$function_spec"

if function_requested ncm; then
  [[ "$ncm_ifname" =~ ^[[:alnum:]_.-]+$ && "${#ncm_ifname}" -le 15 ]] \
    || die "invalid SCARLET_NCM_IFNAME '$ncm_ifname'"
fi

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "must run as root"
}

ensure_configfs() {
  command -v mountpoint >/dev/null 2>&1 || die "mountpoint is required"
  if ! mountpoint -q /sys/kernel/config; then
    mount -t configfs none /sys/kernel/config
  fi
  /usr/sbin/modprobe libcomposite 2>/dev/null || true
  if function_requested mass_storage; then
    /usr/sbin/modprobe usb_f_mass_storage 2>/dev/null || true
  fi
  if function_requested ncm; then
    /usr/sbin/modprobe usb_f_ncm 2>/dev/null || die "usb_f_ncm kernel module is unavailable"
  fi
  if function_requested hid; then
    ensure_hid_module
  fi
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
  local current=""
  if [[ -e "$path" ]]; then
    # Some configfs function attributes (notably NCM MAC addresses) expose a
    # trailing NUL. Strip it before the shell comparison; command substitution
    # cannot carry NUL bytes and would otherwise emit a warning on every boot.
    current=$(tr -d '\000\n' <"$path" 2>/dev/null || true)
  fi
  if [[ "$current" != "$value" ]]; then
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
  install -d -m 0755 "$strings" "$config_strings" "$config"
  function_requested mass_storage && install -d -m 0755 "$mass_function"
  function_requested ncm && install -d -m 0755 "$ncm_function"
  function_requested hid && install -d -m 0755 "$hid_function"
  serial=$(tr -d '\n' </etc/machine-id 2>/dev/null || true)
  [[ -n "$serial" ]] || serial="scarlet-pi"

  write_value "$gadget_root/idVendor" 0x1d6b
  write_value "$gadget_root/idProduct" 0x0104
  write_value "$gadget_root/bcdDevice" 0x0100
  write_value "$gadget_root/bcdUSB" 0x0200
  write_value "$strings/manufacturer" "Raspberry Pi"
  write_value "$strings/product" "Scarlet USB composite"
  write_value "$strings/serialnumber" "$serial"
  write_value "$config_strings/configuration" "Scarlet storage and network"
  write_value "$config/MaxPower" 500
}

hid_report_descriptor() {
  # USB HID boot keyboard: modifiers, reserved byte, six key slots.
  # Keep this as one printf write: configfs treats each write to report_desc
  # as a complete replacement of the descriptor.
  printf '\x05\x01\x09\x06\xa1\x01\x05\x07\x19\xe0\x29\xe7\x15\x00\x25\x01\x75\x01\x95\x08\x81\x02\x95\x01\x75\x08\x81\x01\x95\x05\x75\x01\x05\x08\x19\x01\x29\x05\x91\x02\x95\x01\x75\x03\x91\x01\xc0'
}

ensure_hid() {
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

remove_ncm() {
  local function_path
  for function_path in "$gadget_root/functions"/ncm.*; do
    [[ -d "$function_path" ]] || continue
    rmdir "$function_path" || die "failed to remove disabled NCM function"
  done
}

remove_stale_ncm_functions() {
  local function_path
  for function_path in "$gadget_root/functions"/ncm.*; do
    [[ -d "$function_path" && "$function_path" != "$ncm_function" ]] || continue
    rmdir "$function_path" || die "failed to remove stale NCM function"
  done
}

clear_config_links() {
  local entry
  for entry in "$config_root"/*; do
    [[ -L "$entry" ]] || continue
    rm -f -- "$entry"
  done
}

link_function() {
  local function_name="$1"
  local function_path
  case "$function_name" in
    mass_storage) function_path="functions/mass_storage.0" ;;
    ncm) function_path="functions/ncm.$ncm_ifname" ;;
    hid) function_path="functions/hid.usb0" ;;
    *) die "cannot link unsupported USB gadget function '$function_name'" ;;
  esac
  (cd "$gadget_root" && ln -s "$function_path" "$config_root/$(basename "$function_path")")
}

configure_ncm() {
  function_requested ncm || return 0
  local dev_addr="${SCARLET_NCM_DEV_ADDR:-02:53:43:4e:43:01}"
  local host_addr="${SCARLET_NCM_HOST_ADDR:-02:53:43:4e:43:02}"
  local max_segment_size="${SCARLET_NCM_MAX_SEGMENT_SIZE:-1514}"
  local qmult="${SCARLET_NCM_QMULT:-5}"

  [[ -d "$ncm_function" ]] || die "CDC-NCM function directory is unavailable"
  write_value "$ncm_function/dev_addr" "$dev_addr"
  write_value "$ncm_function/host_addr" "$host_addr"
  # `ifname` is a read-only configfs attribute on Raspberry Pi's usb_f_ncm;
  # the instance name (`ncm.$ncm_ifname`) determines the netdev name instead.
  write_value "$ncm_function/max_segment_size" "$max_segment_size"
  write_value "$ncm_function/qmult" "$qmult"
}

configure_hid() {
  function_requested hid || return 0
  ensure_hid
}

configure_functions() {
  # Functions are immutable while linked/bound. Always unbind and remove all
  # links before changing the composition, then recreate only requested attrs.
  clear_config_links
  if ! function_requested ncm; then
    remove_ncm
  else
    remove_stale_ncm_functions
  fi
  if ! function_requested hid; then
    remove_hid
  fi

  if function_requested mass_storage; then
    install -d -m 0755 "$mass_function"
    write_value "$mass_function/stall" 0
    write_value "$mass_lun/removable" 1
    write_value "$mass_lun/ro" 0
  fi
  configure_ncm
  configure_hid
  for function_name in "${gadget_functions[@]}"; do
    link_function "$function_name"
  done
}

configure_ncm_network() {
  function_requested ncm || return 0
  local ifname="${SCARLET_NCM_IFNAME:-usb0}"
  [[ "${SCARLET_NCM_CONFIGURE_NETWORK:-1}" == 0 ]] && return 0
  command -v ip >/dev/null 2>&1 || {
    echo "scarlet-gadget: warning: ip command unavailable; leaving $ifname unconfigured" >&2
    return 0
  }

  local address="${SCARLET_NCM_PI_ADDRESS:-192.168.88.1/30}"
  local ifpath="/sys/class/net/$ifname"
  for _ in {1..100}; do
    [[ -e "$ifpath" ]] && break
    sleep 0.1
  done
  if [[ ! -e "$ifpath" ]]; then
    echo "scarlet-gadget: warning: NCM interface $ifname is not present yet" >&2
    return 0
  fi
  if ! /usr/sbin/ip link set dev "$ifname" up; then
    echo "scarlet-gadget: warning: failed to bring NCM interface $ifname up" >&2
    return 0
  fi
  if ! /usr/sbin/ip addr replace "$address" dev "$ifname"; then
    echo "scarlet-gadget: warning: failed to assign $address to $ifname" >&2
  fi
}

install_replacement_image() {
  local incoming="$1"
  [[ "$incoming" != "$current_image" ]] || return 0

  # A normal rename is atomic when both paths share a filesystem. The Pi's
  # image lives on /run (tmpfs), while deploy uploads to /var/tmp (disk), so
  # stage the old image off tmpfs before copying the replacement in. This
  # keeps peak RAM usage at one image and leaves a rollback copy if the copy
  # fails halfway through.
  if [[ -e "$current_image" ]] && [[ "$(stat -c '%d' "$incoming" 2>/dev/null || true)" != "$(stat -c '%d' "$current_image" 2>/dev/null || true)" ]]; then
    local backup_dir="${SCARLET_REPLACE_BACKUP_DIR:-/var/tmp}"
    local backup_path
    install -d -m 0755 "$backup_dir"
    backup_path=$(mktemp "$backup_dir/scarlet-current.XXXXXX")
    rm -f -- "$backup_path"

    if ! mv -f -- "$current_image" "$backup_path"; then
      rm -f -- "$backup_path"
      return 1
    fi
    if mv -f -- "$incoming" "$current_image"; then
      rm -f -- "$backup_path"
      return 0
    fi

    # Restore the previous image before reporting failure. The gadget remains
    # detached here; the caller will attempt to re-attach it.
    rm -f -- "$current_image"
    mv -f -- "$backup_path" "$current_image" || true
    return 1
  fi

  mv -f -- "$incoming" "$current_image"
}

deconfigure_ncm_network() {
  function_requested ncm || return 0
  [[ "${SCARLET_NCM_CONFIGURE_NETWORK:-1}" == 0 ]] && return 0
  command -v ip >/dev/null 2>&1 || return 0
  local ifname="${SCARLET_NCM_IFNAME:-usb0}"
  local address="${SCARLET_NCM_PI_ADDRESS:-192.168.88.1/30}"
  [[ -e "/sys/class/net/$ifname" ]] || return 0
  /usr/sbin/ip addr del "$address" dev "$ifname" 2>/dev/null || true
  /usr/sbin/ip link set dev "$ifname" down 2>/dev/null || true
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
  deconfigure_ncm_network
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
  configure_functions
  printf '%s' "$image" > "$mass_lun/file"
  printf '%s' "$udc" > "$gadget_root/UDC"
  configure_ncm_network
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
  install -d -m 0755 "$runtime_dir"
  if ! install_replacement_image "$incoming"; then
    echo "scarlet-gadget: replacement copy failed; restoring the previous image" >&2
    if [[ -f "$current_image" ]]; then
      attach "$current_image" || true
    fi
    die "failed to install replacement image"
  fi
  sync
  attach "$current_image"
}

status() {
  ensure_gadget
  local udc=""
  local function_name
  [[ -e "$gadget_root/UDC" ]] && udc=$(cat "$gadget_root/UDC" 2>/dev/null || true)
  printf 'gadget=%s\n' "$gadget_name"
  printf 'functions=%s\n' "$(IFS=,; echo "${gadget_functions[*]}")"
  printf 'udc=%s\n' "$udc"
  printf 'image=%s\n' "$current_image"
  if [[ -e "$mass_lun/file" ]]; then
    printf 'lun_file=%s\n' "$(cat "$mass_lun/file" 2>/dev/null || true)"
  fi
  for function_name in "${gadget_functions[@]}"; do
    local function_path
    case "$function_name" in
      mass_storage) function_path="$mass_function" ;;
      ncm) function_path="$ncm_function" ;;
      hid) function_path="$hid_function" ;;
    esac
    printf 'function_%s=%s\n' "$function_name" "$( [[ -L "$config_root/$(basename "$function_path")" ]] && echo configured || echo detached )"
  done
  if function_requested ncm; then
    printf 'ncm_ifname=%s\n' "${SCARLET_NCM_IFNAME:-usb0}"
    printf 'ncm_pi_address=%s\n' "${SCARLET_NCM_PI_ADDRESS:-192.168.88.1/30}"
  fi
  if function_requested hid; then
    printf 'hid_device=%s\n' "$( [[ -c "$hid_device" ]] && echo "$hid_device" || echo unavailable )"
  fi
}

send_key() {
  local key="${1:-}"
  function_requested hid || die "HID support is disabled"
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
The default composition is `mass_storage,ncm`; keyboard input is sent through
the Chromebook EC over CCD. Add `hid` to SCARLET_GADGET_FUNCTIONS only when
the Pi-side HID function is intentionally needed.
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
