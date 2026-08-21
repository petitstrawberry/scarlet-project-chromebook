#!/bin/sh
# Build the patched U-Boot payload used by Google CoachZ (SC7180/Trogdor).
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
source_dir=${UBOOT_SOURCE_DIR:-"$project_root/.cache/u-boot"}
output_dir=${UBOOT_BUILD_DIR:-"$source_dir/build-coachz"}
defconfig=${UBOOT_DEFCONFIG:-chromebook_coachz_defconfig}
cross_compile=${CROSS_COMPILE:-aarch64-unknown-linux-gnu-}
jobs=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '4')}

usage() {
	cat <<'EOF'
Usage: build-u-boot-coachz.sh [--jobs N]

Builds U-Boot as a Depthcharge RW_LEGACY payload. The source tree is the
ignored, locally patched checkout in .cache/u-boot and the output is written
to .cache/u-boot/build-coachz by default.

Environment overrides: UBOOT_SOURCE_DIR, UBOOT_BUILD_DIR, UBOOT_DEFCONFIG,
CROSS_COMPILE, JOBS.
EOF
}

while [ "$#" -gt 0 ]; do
	case "$1" in
		--jobs)
			[ "$#" -ge 2 ] || { echo 'error: --jobs requires a value' >&2; exit 2; }
			jobs=$2
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

[ -f "$source_dir/Makefile" ] || {
	echo "error: U-Boot source checkout not found: $source_dir" >&2
	echo "       prepare .cache/u-boot and apply the Trogdor series first" >&2
	exit 1
}
command -v "${cross_compile}gcc" >/dev/null 2>&1 || {
	echo "error: cross compiler not found: ${cross_compile}gcc" >&2
	exit 1
}

mkdir -p "$output_dir"
make -C "$source_dir" O="$output_dir" ARCH=arm \
	CROSS_COMPILE="$cross_compile" "$defconfig"

# Keep the visible UART terminal focused on the current boot. This affects
# ANSI-capable viewers such as minicom only; raw capture files retain history.
"$source_dir/scripts/config" --file "$output_dir/.config" \
	--enable CMD_CLS \
	--set-str BOOTCOMMAND 'cls; bootflow scan -b'
make -C "$source_dir" O="$output_dir" ARCH=arm \
	CROSS_COMPILE="$cross_compile" olddefconfig

# Darwin's clang defaults to -fno-common, while U-Boot's Mach-O host-tool
# section boundary declarations intentionally use common symbols. This flag is
# harmless on GCC/Linux and keeps the same command reproducible on both hosts.
host_cflags=${HOSTCFLAGS:--fcommon}
make -C "$source_dir" O="$output_dir" ARCH=arm \
	CROSS_COMPILE="$cross_compile" HOSTCFLAGS="$host_cflags" -j"$jobs"

artifact="$output_dir/u-boot.elf"
[ -s "$artifact" ] || {
	echo "error: build completed without $artifact" >&2
	exit 1
}

# The built DTB is U-Boot's control plane and intentionally stays USB2/HS.
# Build a separate OS handoff DTB for Scarlet rather than weakening that
# limitation in U-Boot itself.
control_dtb="$output_dir/dts/upstream/src/arm64/qcom/sc7180-trogdor-coachz-r3.dtb"
handoff_builder="$project_root/projects/aarch64-coachz-limine/tools/build-os-handoff-dtb.sh"
[ -x "$handoff_builder" ] || {
	echo "error: CoachZ OS handoff DTB builder is not executable: $handoff_builder" >&2
	exit 1
}
"$handoff_builder" --source "$control_dtb" --output "$output_dir/scarlet-os-handoff.dtb"

printf 'U-Boot CoachZ payload: %s\n' "$artifact"
if command -v sha256sum >/dev/null 2>&1; then
	sha256sum "$artifact"
elif command -v shasum >/dev/null 2>&1; then
	shasum -a 256 "$artifact"
fi
printf 'default bootcmd: '
bootcmd=$(sed -n 's/^CONFIG_BOOTCOMMAND="\(.*\)"/\1/p' "$output_dir/.config")
printf '%s\n' "$bootcmd"
printf 'default bootdelay: '
bootdelay=$(sed -n 's/^CONFIG_BOOTDELAY=//p' "$output_dir/.config")
printf '%s\n' "$bootdelay"
[ "$bootcmd" = 'cls; bootflow scan -b' ] || {
	echo 'error: CoachZ payload is not configured for bootflow auto-boot' >&2
	exit 1
}
[ "$bootdelay" = '0' ] || {
	echo 'error: CoachZ payload must have CONFIG_BOOTDELAY=0' >&2
	exit 1
}

for expected in \
	'CONFIG_CMD_CLS=y' \
	'CONFIG_DEBUG_UART=y' \
	'CONFIG_DEBUG_UART_ANNOUNCE=y' \
	'CONFIG_DEBUG_UART_BASE=0xa88000' \
	'CONFIG_DEBUG_UART_MSM_GENI=y' \
	'CONFIG_DEBUG_UART_CLOCK=36864000' \
	'CONFIG_DEBUG_UART_BOARD_INIT=y' \
	'CONFIG_QCOM_GENI=y' \
	'CONFIG_MSM_GENI_SERIAL=y'; do
	grep -Fxq "$expected" "$output_dir/.config" || {
		echo "error: CoachZ debug UART setting missing: $expected" >&2
		exit 1
	}
done
