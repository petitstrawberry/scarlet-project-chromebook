#!/bin/sh
set -eu

project_dir=${SCARLET_LINUX_PROJECT:-projects/aarch64-chromebook-smoke}
profile=debug
release_flag=
if [ "${1:-}" = "--release" ]; then
    profile=release
    release_flag=--release
    shift
fi
if [ "$#" -ne 0 ]; then
    echo "usage: $0 [--release]" >&2
    exit 2
fi

cargo scarlet image --project "$project_dir" $release_flag

elf="$project_dir/bsp/target/aarch64-linux-image/$profile/scarlet"
output="$project_dir/.scarlet/images/Image"
mkdir -p "$(dirname "$output")"
python3 scripts/elf-to-linux-image.py "$elf" "$output"
python3 scripts/validate-linux-image.py "$output"
printf '%s\n' "$output"
