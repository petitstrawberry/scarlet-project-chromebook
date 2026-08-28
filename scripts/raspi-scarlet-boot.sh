#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
delay_ms="${SCARLET_ALTFW_KEY_DELAY_MS:-5000}"
key="${SCARLET_ALTFW_KEY:-ctrl-l}"
device="${SCARLET_CCD_DEVICE:-18d1:5014}"
serial="${SCARLET_CCD_SERIAL:-}"

usage() {
  cat >&2 <<'EOF'
usage: raspi-scarlet-boot.sh [options]

Reset the Chromebook through the Mac's CCD EC console, then inject the
selected keyboard shortcut after the Developer Mode screen has appeared.

Options:
  --delay-ms MS       Delay after apreset before sending the key (default: 5000).
  --key KEY           ctrl-l, enter, ctrl-d, or release (default: ctrl-l).
  --device VID:PID    CCD USB device (default: 18d1:5014).
  --serial SERIAL     CCD serial number passed to the EC one-shot helper.
  -h, --help          Show this help text.

Environment overrides: SCARLET_ALTFW_KEY_DELAY_MS, SCARLET_ALTFW_KEY,
SCARLET_CCD_DEVICE, SCARLET_CCD_SERIAL.
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

case "$delay_ms" in
  ''|*[!0-9]*) die "delay must be a non-negative integer in milliseconds" ;;
esac
valid_key "$key" || die "unknown key '$key'"

ec_args=(--interface 2 --device "$device")
if [[ -n "$serial" ]]; then
  ec_args+=(--serial "$serial")
fi

echo "Resetting Chromebook through CCD EC (apreset)..." >&2
printf 'apreset\n' | "$script_dir/ec-usb-command.py" "${ec_args[@]}"

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
echo "Reset/key sequence started." >&2
