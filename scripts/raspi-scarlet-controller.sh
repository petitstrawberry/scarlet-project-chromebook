#!/usr/bin/env bash
set -euo pipefail

config_env="${SCARLET_CONFIG_FILE:-/etc/default/scarlet-gadget}"
if [[ -r "$config_env" ]]; then
  . "$config_env"
fi

gadget_command="${SCARLET_GADGET_COMMAND:-/usr/local/sbin/scarlet-gadget}"
runtime_dir="${SCARLET_RUNTIME_DIR:-/run/scarlet}"
current_image="${SCARLET_CURRENT_IMAGE:-$runtime_dir/current.img}"

die() {
  echo "scarlet-controller: $*" >&2
  exit 1
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "must run as root"
}

send_key() {
  local key=${1:-}
  [[ -n "$key" ]] || die "key is required"
  "$gadget_command" key "$key"
}

key_after() {
  local delay_ms=${1:-}
  local key=${2:-}
  [[ -n "$delay_ms" && -n "$key" ]] || die "usage: key-after DELAY_MS KEY"
  case "$delay_ms" in
    ''|*[!0-9]*) die "delay must be a non-negative integer in milliseconds" ;;
  esac
  local delay_seconds
  delay_seconds=$(printf '%s.%03d' "$((delay_ms / 1000))" "$((delay_ms % 1000))")
  sleep "$delay_seconds"
  send_key "$key"
}

status() {
  "$gadget_command" status
  if [[ -f "$current_image" ]]; then
    printf 'image_size=%s\n' "$(stat -c '%s' "$current_image")"
    printf 'image_sha256=%s\n' "$(sha256sum "$current_image" | awk '{print $1}')"
  else
    printf 'image_size=missing\n'
    printf 'image_sha256=missing\n'
  fi
  printf 'controller=ssh-command\n'
  if [[ "${hid_enabled:-0}" == 1 ]]; then
    printf 'keys=ctrl-l,enter,ctrl-d,release\n'
  else
    printf 'keys=disabled (EC keyboard injection is active on host)\n'
  fi
}

usage() {
  cat >&2 <<'EOF'
usage: scarlet-controller {status|key|key-after} [argument...]

Commands:
  status
  key KEY                         Send one HID key report.
  key-after DELAY_MS KEY          Sleep, then send one HID key report.

KEY is one of ctrl-l, enter, ctrl-d, or release.
EOF
}

require_root
case "${1:-}" in
  status)
    status
    ;;
  key)
    send_key "${2:-}"
    ;;
  key-after)
    key_after "${2:-}" "${3:-}"
    ;;
  *)
    usage
    exit 2
    ;;
esac
