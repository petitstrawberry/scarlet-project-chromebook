#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/raspi-ssh.sh"
usage() {
  cat >&2 <<'EOF'
usage: raspi-scarlet-control.sh COMMAND [ARGUMENTS]
  status                          Gadget, network, and CCD status.
  reset                           Reset AP through the Pi's CCD.
  boot [DELAY_MS [KEY]]            Reset, wait (default 5000 ms), then Ctrl+L.
  key KEY                         Inject an EC keyboard shortcut.
  key-after DELAY_MS KEY           Wait, then inject a key.
  schedule-key DELAY_MS KEY        Run delayed key over background SSH.
  console {ap|ec|cr50}             Interactive CCD console (SSH escape: ~.).
  log {ap|ec|cr50}                 Last 64 KiB received by the service.
  send {ap|ec|cr50} TEXT           Send a console command plus newline.
Keys: ctrl-l, ctrl-d, enter, release. Delay: 0..120000 ms.
Settings: SCARLET_RPI_HOST/USER/IDENTITY, or ~/.config/scarlet/raspi.env.
EOF
}
die() { echo "raspi-scarlet-control: $*" >&2; exit 2; }
valid_key() { case "$1" in ctrl-l|ctrl-d|enter|release) ;; *) die "invalid key: $1";; esac; }
valid_delay() {
  [[ "$1" =~ ^[0-9]{1,6}$ ]] || die 'delay must be 0..120000 ms'
  ((10#$1 <= 120000)) || die 'delay must be 0..120000 ms'
}
valid_target() { case "$1" in ap|ec|cr50) ;; *) die "invalid target: $1";; esac; }
command_name=${1:-}
shift || true
background=false
interactive=false
case "$command_name" in
  -h|--help) usage; exit 0;;
  status|reset)
    [[ $# -eq 0 ]] || die "$command_name takes no arguments"
    remote_command=(sudo -n /usr/local/sbin/scarlet-controller "$command_name")
    ;;
  boot)
    [[ $# -le 2 ]] || die 'usage: boot [DELAY_MS [KEY]]'
    delay=${1:-5000}; key=${2:-ctrl-l}; valid_delay "$delay"; valid_key "$key"
    remote_command=(sudo -n /usr/local/sbin/scarlet-ccd boot --delay-ms "$((10#$delay))" --key "$key")
    ;;
  key|key-after|schedule-key)
    if [[ "$command_name" == key ]]; then
      [[ $# -eq 1 ]] || die 'usage: key KEY'
      delay=0; key=$1
    else
      [[ $# -eq 2 ]] || die "usage: $command_name DELAY_MS KEY"
      delay=$1; key=$2
    fi
    valid_delay "$delay"; valid_key "$key"
    remote_command=(sudo -n /usr/local/sbin/scarlet-ccd key --delay-ms "$((10#$delay))" --key "$key")
    [[ "$command_name" != schedule-key ]] || background=true
    ;;
  console|log|send)
    if [[ "$command_name" == send ]]; then
      [[ $# -eq 2 ]] || die 'usage: send TARGET TEXT'
    else
      [[ $# -eq 1 ]] || die "usage: $command_name TARGET"
    fi
    valid_target "$1"
    remote_command=(sudo -n /usr/local/sbin/scarlet-ccd "$command_name" --target "$1")
    [[ "$command_name" != send ]] || remote_command+=(--text "$2")
    [[ "$command_name" != console ]] || interactive=true
    ;;
  *) usage; exit 2;;
esac
# Raspberry Pi OS uses bash; %q preserves literal console text across SSH.
printf -v remote_line '%q ' "${remote_command[@]}"
if [[ "$interactive" == true && -t 0 ]]; then
  exec ssh -tt "${ssh_opts[@]}" "$remote" "$remote_line"
elif [[ "$background" == true ]]; then
  exec ssh -f -n "${ssh_opts[@]}" "$remote" "$remote_line"
else
  exec ssh -T "${ssh_opts[@]}" "$remote" "$remote_line"
fi
