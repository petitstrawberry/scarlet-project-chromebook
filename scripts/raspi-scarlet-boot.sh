#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
delay_ms="${SCARLET_ALTFW_KEY_DELAY_MS:-5000}"
key="${SCARLET_ALTFW_KEY:-ctrl-l}"
device="${SCARLET_CCD_DEVICE:-18d1:5014}"
serial="${SCARLET_CCD_SERIAL:-}"
reset_ap=true
backend="${SCARLET_CCD_BACKEND:-remote}"

usage() {
  cat >&2 <<'EOF'
usage: raspi-scarlet-boot.sh [options]

Reset the Chromebook through the Pi's CCD service, then inject the
selected keyboard shortcut after the Developer Mode screen has appeared.

Options:
  --backend MODE     remote (default), or local for a CCD connected to this Mac.
  --no-reset         Send the key without resetting an AP already at the boot menu.
  --delay-ms MS       Delay after apreset, 0..120000 ms (default: 5000).
  --key KEY           ctrl-l, enter, ctrl-d, or release (default: ctrl-l).
  --device VID:PID    CCD USB device (default: 18d1:5014).
  --serial SERIAL     CCD serial number passed to the EC one-shot helper.
  -h, --help          Show this help text.

Environment overrides: SCARLET_ALTFW_KEY_DELAY_MS, SCARLET_ALTFW_KEY,
SCARLET_CCD_DEVICE, SCARLET_CCD_SERIAL, SCARLET_CCD_BACKEND, SCARLET_RPI_HOST.
Device/serial options apply only to the local backend; configure the Pi service separately.
EOF
}

die() {
  echo "raspi-scarlet-boot: $*" >&2
  exit 2
}

valid_key() {
  case "${1:-}" in
    ctrl-l|enter|ctrl-d|release) return 0 ;;
    *) return 1 ;;
  esac
}

while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --backend)
      [[ "$#" -ge 2 ]] || die "--backend requires remote or local"
      backend=$2
      shift 2
      ;;
    --no-reset)
      reset_ap=false
      shift
      ;;
    --delay-ms)
      [[ "$#" -ge 2 ]] || die '--delay-ms requires milliseconds'
      delay_ms=$2
      shift 2
      ;;
    --key)
      [[ "$#" -ge 2 ]] || die '--key requires ctrl-l, enter, ctrl-d, or release'
      key=$2
      shift 2
      ;;
    --device)
      [[ "$#" -ge 2 ]] || die '--device requires VID:PID'
      device=$2
      shift 2
      ;;
    --serial)
      [[ "$#" -ge 2 ]] || die '--serial requires a value'
      serial=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ "$delay_ms" =~ ^[0-9]{1,6}$ ]] || die "delay must be 0..120000 ms"
delay_ms=$((10#$delay_ms))
((delay_ms <= 120000)) || die "delay must be 0..120000 ms"
valid_key "$key" || die "unknown key '$key'"

case "$backend" in
  remote)
    if [[ "$reset_ap" == true ]]; then
      exec "$script_dir/raspi-scarlet-control.sh" boot "$delay_ms" "$key"
    else
      exec "$script_dir/raspi-scarlet-control.sh" key-after "$delay_ms" "$key"
    fi
    ;;
  local) ;;
  *) die "backend must be remote or local" ;;
esac

ec_args=(--interface 2 --device "$device")
if [[ -n "$serial" ]]; then
  ec_args+=(--serial "$serial")
fi

if [[ "$reset_ap" == true ]]; then
  echo "Resetting Chromebook through CCD EC (apreset)..." >&2
  printf 'apreset\n' | "$script_dir/ec-usb-command.py" "${ec_args[@]}"
fi

wait_before_key() {
  local delay_seconds
  delay_seconds=$(printf '%d.%03d' "$((delay_ms / 1000))" "$((delay_ms % 1000))")
  sleep "$delay_seconds"
}

send_ec_key() {
  local key_name=${1:-}
  # The EC console's kbpress syntax is column, row, pressed. CoachZ uses the
  # default matrix: left Ctrl=(row 2,col 0), L=(row 4,col 9), D=(row 4,col 2),
  # and Enter=(row 4,col 11).
  case "$key_name" in
    ctrl-l)
      printf 'kbpress 0 2 1\nkbpress 9 4 1\nkbpress 9 4 0\nkbpress 0 2 0\n' |
        "$script_dir/ec-usb-command.py" "${ec_args[@]}"
      ;;
    ctrl-d)
      printf 'kbpress 0 2 1\nkbpress 2 4 1\nkbpress 2 4 0\nkbpress 0 2 0\n' |
        "$script_dir/ec-usb-command.py" "${ec_args[@]}"
      ;;
    enter)
      printf 'kbpress 11 4 1\nkbpress 11 4 0\n' |
        "$script_dir/ec-usb-command.py" "${ec_args[@]}"
      ;;
    release)
      printf 'kbpress clear\n' | "$script_dir/ec-usb-command.py" "${ec_args[@]}"
      ;;
    *)
      die "cannot inject unsupported key '$key_name' through the EC backend"
      ;;
  esac
}

echo "Waiting $delay_ms ms, then injecting EC keyboard key '$key'..." >&2
wait_before_key
send_ec_key "$key"
echo "Boot key sequence sent." >&2
