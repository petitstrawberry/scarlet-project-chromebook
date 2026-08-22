#!/usr/bin/env bash
# Build the DTB passed from U-Boot to Scarlet without changing U-Boot's
# deliberately high-speed-only control DTB or its disabled trackpad bus.
set -euo pipefail

project_dir="${SCARLET_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
repo_root="$(cd "$project_dir/../.." && pwd)"
default_build_dir="${UBOOT_BUILD_DIR:-$repo_root/.cache/u-boot/build-coachz}"
source_dtb="${UBOOT_COACHZ_CONTROL_DTB:-$default_build_dir/dts/upstream/src/arm64/qcom/sc7180-trogdor-coachz-r3.dtb}"
output_dtb="${SCARLET_COACHZ_OS_DTB:-$default_build_dir/scarlet-os-handoff.dtb}"

usage() {
  cat <<'EOF'
Usage: build-os-handoff-dtb.sh [--source CONTROL_DTB] [--output OS_DTB]

Copy U-Boot's CoachZ control DTB, restore the USB3 handoff description, and
enable the Elan trackpad I2C bus for Scarlet. The source must remain the
U-Boot-only QUSB2/high-speed DTB with the trackpad bus disabled; this script
never modifies it.

Environment overrides: SCARLET_PROJECT_DIR, UBOOT_BUILD_DIR,
UBOOT_COACHZ_CONTROL_DTB, SCARLET_COACHZ_OS_DTB.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)
      [[ $# -ge 2 ]] || { echo 'error: --source requires a path' >&2; exit 2; }
      source_dtb="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || { echo 'error: --output requires a path' >&2; exit 2; }
      output_dtb="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      exit 2
      ;;
  esac
done

for tool in fdtget fdtput; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: required FDT tool not found: $tool" >&2
    exit 1
  }
done
[[ -s "$source_dtb" ]] || {
  echo "error: U-Boot CoachZ control DTB not found: $source_dtb" >&2
  exit 1
}

qmp_node='/soc@0/phy@88e8000'
dwc3_node='/soc@0/usb@a6f8800/usb@a600000'
hsphy_node='/soc@0/phy@88e3000'
trackpad_i2c_node='/soc@0/geniqup@ac0000/i2c@a84000'
trackpad_node="$trackpad_i2c_node/trackpad@15"

# Guard the boundary explicitly: the build input is U-Boot's HS-only control
# DTB, while only the copied output receives Scarlet's USB3 description.
[[ "$(fdtget -t s "$source_dtb" "$qmp_node" status)" == 'disabled' ]] || {
  echo 'error: control DTB QMP PHY must remain disabled for U-Boot' >&2
  exit 1
}
[[ "$(fdtget -t s "$source_dtb" "$dwc3_node" maximum-speed)" == 'high-speed' ]] || {
  echo 'error: control DTB DWC3 must remain high-speed for U-Boot' >&2
  exit 1
}
[[ "$(fdtget -t s "$source_dtb" "$dwc3_node" phy-names)" == 'usb2-phy' ]] || {
  echo 'error: control DTB DWC3 must expose only usb2-phy for U-Boot' >&2
  exit 1
}

hsphy_phandle="$(fdtget -t x "$source_dtb" "$hsphy_node" phandle)"
qmp_phandle="$(fdtget -t x "$source_dtb" "$qmp_node" phandle)"
read -r hsphy_cfg_clock_phandle hsphy_cfg_clock_id hsphy_ref_clock_phandle hsphy_ref_clock_id hsphy_extra_clock_cells < <(
  fdtget -t x "$source_dtb" "$hsphy_node" clocks
)
[[ -n "$hsphy_cfg_clock_phandle" && -n "$hsphy_cfg_clock_id" && -n "$hsphy_ref_clock_phandle" && -n "$hsphy_ref_clock_id" && -z "${hsphy_extra_clock_cells:-}" ]] || {
  echo 'error: CoachZ QUSB2 PHY clock binding is unexpected' >&2
  exit 1
}
[[ "$(fdtget -t s "$source_dtb" "$hsphy_node" clock-names)" == 'cfg_ahb ref' ]] || {
  echo 'error: CoachZ QUSB2 PHY clock names are unexpected' >&2
  exit 1
}
[[ "$(fdtget -t x "$source_dtb" "$qmp_node" '#phy-cells')" == '1' ]] || {
  echo 'error: CoachZ QMP USB3 PHY must expose one PHY selector cell' >&2
  exit 1
}
[[ "$(fdtget -t x "$source_dtb" "$dwc3_node" phys)" == "$hsphy_phandle" ]] || {
  echo 'error: control DTB DWC3 USB2 PHY binding is unexpected' >&2
  exit 1
}
[[ "$(fdtget -t s "$source_dtb" "$trackpad_i2c_node" status)" == 'disabled' ]] || {
  echo 'error: control DTB Elan trackpad I2C bus must remain disabled for U-Boot' >&2
  exit 1
}
[[ "$(fdtget -t s "$source_dtb" "$trackpad_node" compatible)" == 'elan,ekth3000' ]] || {
  echo 'error: CoachZ Elan trackpad binding is missing or unexpected' >&2
  exit 1
}
[[ "$(fdtget -t x "$source_dtb" "$trackpad_node" reg)" == '15' ]] || {
  echo 'error: CoachZ Elan trackpad address is unexpected' >&2
  exit 1
}

mkdir -p "$(dirname "$output_dtb")"
temporary_output="$(mktemp "${output_dtb}.tmp.XXXXXX")"
trap 'rm -f -- "$temporary_output"' EXIT
cp "$source_dtb" "$temporary_output"

# QMP_USB43DP_USB3_PHY is selector 0. Resolve both phandles from the control
# DTB so this stays correct if the source DTB's allocation changes.
fdtput -t s "$temporary_output" "$qmp_node" status okay
fdtput -t x "$temporary_output" "$dwc3_node" phys "$hsphy_phandle" "$qmp_phandle" 0
fdtput -t s "$temporary_output" "$dwc3_node" phy-names usb2-phy usb3-phy
fdtput -t s "$temporary_output" "$dwc3_node" maximum-speed super-speed
fdtput -t s "$temporary_output" "$trackpad_i2c_node" status okay

# Scarlet has the SC7180 GCC provider for the AHB programming clock, but not
# an RPMh CXO provider.  Depthcharge/U-Boot leaves CXO running, and the QUSB2
# driver preserves that firmware handoff.  Omit the unmanaged `ref` specifier
# from the OS-only DTB so generic provider parsing cannot reject the PHY before
# the driver's handoff policy runs.
fdtput -t x "$temporary_output" "$hsphy_node" clocks "0x$hsphy_cfg_clock_phandle" "0x$hsphy_cfg_clock_id"
fdtput -t s "$temporary_output" "$hsphy_node" clock-names cfg_ahb

[[ "$(fdtget -t s "$temporary_output" "$qmp_node" status)" == 'okay' ]]
[[ "$(fdtget -t x "$temporary_output" "$dwc3_node" phys)" == "$hsphy_phandle $qmp_phandle 0" ]]
[[ "$(fdtget -t s "$temporary_output" "$dwc3_node" phy-names)" == 'usb2-phy usb3-phy' ]]
[[ "$(fdtget -t s "$temporary_output" "$dwc3_node" maximum-speed)" == 'super-speed' ]]
[[ "$(fdtget -t x "$temporary_output" "$hsphy_node" clocks)" == "$hsphy_cfg_clock_phandle $hsphy_cfg_clock_id" ]]
[[ "$(fdtget -t s "$temporary_output" "$hsphy_node" clock-names)" == 'cfg_ahb' ]]
[[ "$(fdtget -t s "$temporary_output" "$trackpad_i2c_node" status)" == 'okay' ]]
[[ "$(fdtget -t s "$temporary_output" "$trackpad_node" compatible)" == 'elan,ekth3000' ]]

mv -f "$temporary_output" "$output_dtb"
trap - EXIT

echo "CoachZ U-Boot control DTB: $source_dtb"
echo "CoachZ Scarlet OS handoff DTB: $output_dtb"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$source_dtb" "$output_dtb"
else
  shasum -a 256 "$source_dtb" "$output_dtb"
fi
