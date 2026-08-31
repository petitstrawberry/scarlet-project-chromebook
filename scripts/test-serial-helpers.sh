#!/bin/sh
# Hardware-independent regression tests for the ChromiumOS EC serial helpers.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

expect_failure() {
    name=$1
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'FAIL: %s unexpectedly succeeded\n' "$name" >&2
        exit 1
    fi
    printf 'PASS: %s\n' "$name"
}

/bin/sh -n "$script_dir/fetch-chromiumos-ec.sh"
/bin/sh -n "$script_dir/ec-usb-console.sh"
bash -n "$script_dir/raspi-scarlet-gadget.sh"
bash -n "$script_dir/raspi-scarlet-ncm.sh"
bash -n "$script_dir/mac-scarlet-routing.sh"
bash -n "$script_dir/raspi-scarlet-controller.sh"
bash -n "$script_dir/raspi-scarlet-control.sh"
bash -n "$script_dir/raspi-scarlet-boot.sh"
bash -n "$script_dir/install-raspi-scarlet.sh"
bash -n "$script_dir/deploy-scarlet-to-raspi.sh"
python3 -m py_compile "$script_dir/check-pyusb-libusb.py"
python3 -m py_compile "$script_dir/ec-usb-command.py"
"$script_dir/fetch-chromiumos-ec.sh" --help >/dev/null
"$script_dir/ec-usb-console.sh" --help >/dev/null
"$script_dir/raspi-scarlet-boot.sh" --help >/dev/null 2>&1
"$script_dir/ec-usb-command.py" --help >/dev/null
"$script_dir/check-pyusb-libusb.py" --help >/dev/null

expect_failure 'fetch rejects an empty revision' \
    "$script_dir/fetch-chromiumos-ec.sh" --revision ''
expect_failure 'launcher rejects an invalid target' \
    "$script_dir/ec-usb-console.sh" --target invalid
expect_failure 'launcher rejects malformed VID:PID' \
    "$script_dir/ec-usb-console.sh" --device 18d1:xyz
expect_failure 'diagnostic rejects malformed VID:PID' \
    "$script_dir/check-pyusb-libusb.py" --vidpid invalid
expect_failure 'one-shot console rejects malformed VID:PID' \
    "$script_dir/ec-usb-command.py" --device invalid
expect_failure 'boot launcher rejects an invalid delay' \
    "$script_dir/raspi-scarlet-boot.sh" --delay-ms invalid
expect_failure 'boot launcher rejects an invalid key' \
    "$script_dir/raspi-scarlet-boot.sh" --key invalid
printf 'PASS: serial helper static checks complete\n'
