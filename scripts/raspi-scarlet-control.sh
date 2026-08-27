#!/usr/bin/env bash
set -euo pipefail

rpi_host="${SCARLET_RPI_HOST:-192.168.77.2}"
rpi_user="${SCARLET_RPI_USER:-pi}"
identity_file="${SCARLET_RPI_IDENTITY:-$HOME/.ssh/id_rsa}"

usage() {
  cat >&2 <<'EOF'
usage: raspi-scarlet-control.sh {status|key|key-after|schedule-key} [argument...]

Commands:
  status
  key KEY                         Send ctrl-l, enter, ctrl-d, or release.
  key-after DELAY_MS KEY          Send one key after a delay.
  schedule-key DELAY_MS KEY       Schedule one delayed key and return.
EOF
}

die() {
  echo "raspi-scarlet-control: $*" >&2
  exit 2
}

valid_key() {
  case "${1:-}" in
    ctrl-l|enter|ctrl-d|release) return 0 ;;
    *) return 1 ;;
  esac
}

command_name="${1:-}"
shift || true
background=false
case "$command_name" in
  status)
    [[ "$#" -eq 0 ]] || die "status takes no arguments"
    remote_args=(status)
    ;;
  key)
    [[ "$#" -eq 1 ]] || die "usage: key KEY"
    valid_key "$1" || die "unknown key '$1'"
    remote_args=(key "$1")
    ;;
  key-after)
    [[ "$#" -eq 2 ]] || die "usage: key-after DELAY_MS KEY"
    case "$1" in
      ''|*[!0-9]*) die "delay must be a non-negative integer in milliseconds" ;;
    esac
    valid_key "$2" || die "unknown key '$2'"
    remote_args=(key-after "$1" "$2")
    ;;
  schedule-key)
    [[ "$#" -eq 2 ]] || die "usage: schedule-key DELAY_MS KEY"
    case "$1" in
      ''|*[!0-9]*) die "delay must be a non-negative integer in milliseconds" ;;
    esac
    valid_key "$2" || die "unknown key '$2'"
    remote_args=(key-after "$1" "$2")
    background=true
    ;;
  *)
    usage
    exit 2
    ;;
esac

ssh_opts=(-i "$identity_file" -o BatchMode=yes -o ConnectTimeout=10)
remote="$rpi_user@$rpi_host"
remote_command=(sudo /usr/local/sbin/scarlet-controller "${remote_args[@]}")
printf -v remote_line '%q ' "${remote_command[@]}"
if [[ "$background" == true ]]; then
  # Keep the SSH session alive while the Pi-side controller sleeps, but return
  # to the caller as soon as authentication and command startup succeed.
  ssh -f -n "${ssh_opts[@]}" "$remote" "$remote_line"
else
  ssh "${ssh_opts[@]}" "$remote" "$remote_line"
fi
