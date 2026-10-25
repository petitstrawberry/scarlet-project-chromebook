#!/usr/bin/env bash
set -euo pipefail

config_env="${SCARLET_CONFIG_FILE:-/etc/default/scarlet-gadget}"
if [[ -r "$config_env" ]]; then
  . "$config_env"
fi

gadget_command="${SCARLET_GADGET_COMMAND:-/usr/local/sbin/scarlet-gadget}"
network_command="${SCARLET_NCM_COMMAND:-/usr/local/sbin/scarlet-ncm}"
runtime_dir="${SCARLET_RUNTIME_DIR:-/run/scarlet}"
ccd_command="${SCARLET_CCD_COMMAND:-/usr/local/sbin/scarlet-ccd}"
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
  "$ccd_command" key --key "$key"
}

key_after() {
  local delay_ms=${1:-}
  local key=${2:-}
  [[ -n "$delay_ms" && -n "$key" ]] || die "usage: key-after DELAY_MS KEY"
  "$ccd_command" key --delay-ms "$delay_ms" --key "$key"
}

status() {
  "$gadget_command" status
  if [[ -x "$network_command" ]]; then
    echo '--- ncm gateway ---'
    "$network_command" status || true
  fi
  if [[ -f "$current_image" ]]; then
    printf 'image_size=%s\n' "$(stat -c '%s' "$current_image")"
    printf 'image_sha256=%s\n' "$(sha256sum "$current_image" | awk '{print $1}')"
  else
    printf 'image_size=missing\n'
    printf 'image_sha256=missing\n'
  fi
  printf 'controller=ssh+ccd-service\n'
  "$ccd_command" status
}

usage() {
  cat >&2 <<'EOF'
usage: scarlet-controller {status|reset|key|key-after} [argument...]

Commands:
  status
  reset                           Reset the AP through CCD.
  key KEY                         Send one EC key sequence.
  key-after DELAY_MS KEY          Sleep, then send one EC key sequence.

KEY is one of ctrl-l, enter, ctrl-d, or release.
EOF
}

require_root
case "${1:-}" in
  status)
    status
    ;;
  reset)
    "$ccd_command" reset
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
