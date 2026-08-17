#!/bin/sh
# Launch ChromiumOS EC's USB serial console with CCD-friendly defaults.
set -eu

readonly EC_REVISION_DEFAULT='ab6941cd8b5b973152d6ad947daa43160d8e9d2b'
readonly DEVICE_DEFAULT='18d1:5014'

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
source_dir=${EC_SOURCE_DIR:-"$project_root/.cache/chromiumos-ec"}
revision=${EC_REVISION:-$EC_REVISION_DEFAULT}
device=$DEVICE_DEFAULT
target=cr50
serial=''
forward_ctrl_c=false

usage() {
    cat <<'EOF'
Usage: ec-usb-console.sh [OPTIONS] [-- CONSOLE.PY OPTIONS...]

Launch ChromiumOS EC extra/usb_serial/console.py. If the source checkout is
missing, it is fetched at the pinned default revision first.

Options:
  --target TARGET     Console target: cr50 (interface 0), ap (1), or ec (2)
                      (default: cr50)
  --device VID:PID    USB device (default: 18d1:5014)
  --serial SERIAL     Forward SERIAL to console.py's --serial option
  --forward-ctrl-c    Send byte 0x03 to the device instead of exiting
  --source DIR        ChromiumOS EC source checkout
  --revision REV      Revision used if --source must be fetched
  -h, --help          Show this help text

Environment overrides: EC_SOURCE_DIR, EC_REVISION, PYTHON (default: python3).
All arguments after -- are passed to console.py unchanged.
EOF
}

die() {
    printf '%s\n' "error: $*" >&2
    exit 2
}

valid_vid_pid() {
    case "$1" in
        [0123456789abcdefABCDEF][0123456789abcdefABCDEF][0123456789abcdefABCDEF][0123456789abcdefABCDEF]:[0123456789abcdefABCDEF][0123456789abcdefABCDEF][0123456789abcdefABCDEF][0123456789abcdefABCDEF]) return 0 ;;
        *) return 1 ;;
    esac
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --)
            shift
            break
            ;;
        --target)
            [ "$#" -ge 2 ] || die '--target requires cr50, ap, or ec'
            target=$2
            shift 2
            ;;
        --device)
            [ "$#" -ge 2 ] || die '--device requires VID:PID'
            device=$2
            shift 2
            ;;
        --serial)
            [ "$#" -ge 2 ] || die '--serial requires a value'
            serial=$2
            shift 2
            ;;
        --forward-ctrl-c)
            forward_ctrl_c=true
            shift
            ;;
        --source)
            [ "$#" -ge 2 ] || die '--source requires a directory'
            source_dir=$2
            shift 2
            ;;
        --revision)
            [ "$#" -ge 2 ] || die '--revision requires a revision'
            revision=$2
            shift 2
            ;;
        cr50|ap|ec)
            target=$1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *) die "unknown launcher option: $1 (put console.py options after --)" ;;
    esac
done

case "$target" in
    cr50) interface=0 ;;
    ap) interface=1 ;;
    ec) interface=2 ;;
    *) die "invalid target '$target' (expected cr50, ap, or ec)" ;;
esac
valid_vid_pid "$device" || die "invalid device '$device' (expected VID:PID in hexadecimal)"
[ -n "$revision" ] || die 'revision must not be empty'

console="$source_dir/extra/usb_serial/console.py"
if [ ! -f "$console" ]; then
    "$script_dir/fetch-chromiumos-ec.sh" --dest "$source_dir" --revision "$revision"
fi
[ -f "$console" ] || die "console.py was not found after fetching: $console"

python=${PYTHON:-python3}
command -v "$python" >/dev/null 2>&1 || die "Python interpreter not found: $python"

if [ "$forward_ctrl_c" = true ]; then
    runner="$script_dir/ec-usb-console-forward.py"
    [ -f "$runner" ] || die "Ctrl-C forwarding helper not found: $runner"
    set -- "$runner" "$console" -d "$device" -i "$interface" "$@"
else
    set -- "$console" -d "$device" -i "$interface" "$@"
fi

if [ -n "$serial" ]; then
    set -- "$@" --serialno "$serial"
fi

exec "$python" "$@"
