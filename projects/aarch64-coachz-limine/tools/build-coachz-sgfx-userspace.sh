#!/bin/sh
set -eu

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo "usage: $0 <staging-bin-directory>" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
chromebook_root=$(CDPATH= cd -- "$project_dir/../.." && pwd)
scarlet_root=$(CDPATH= cd -- "$chromebook_root/../Scarlet" && pwd)
boxcraft_root=${BOXCRAFT_ROOT:-"$chromebook_root/../boxcraft"}
destination=$1
case "$destination" in
    /*) ;;
    *) destination="$(pwd)/$destination" ;;
esac

main_lock="$scarlet_root/.cargo/Cargo.lock"
config="$project_dir/coachz-userspace-cargo.toml"
lock_dir="$project_dir/.scarlet/coachz-userspace-lock"
lock_file="$lock_dir/Cargo.lock"
boxcraft_lock_dir="$project_dir/.scarlet/coachz-boxcraft-lock"
boxcraft_lock_file="$boxcraft_lock_dir/Cargo.lock"
target_dir="$project_dir/.scarlet/cache/coachz-userspace-target"
target_bins="$target_dir/aarch64-unknown-scarlet/release"

if [ ! -f "$main_lock" ] || [ ! -f "$config" ]; then
    echo "CoachZ userspace inputs are incomplete" >&2
    exit 1
fi

mkdir -p "$destination" "$lock_dir" "$boxcraft_lock_dir" "$target_dir"
cp "$main_lock" "$lock_file"

echo "cargo-scarlet: rebuilding CoachZ SGFX userspace from $chromebook_root" >&2
cd "$project_dir"
# The copied workspace lock normally pins these crates to the last pushed git
# revision.  Merely supplying a `[patch]` table does not replace an already
# locked git package, so explicitly select the local Adreno backend once.  Its
# path dependencies pull the remaining local A6xx crates into the same lock.
# Without this step a successful image build can silently omit working-tree
# driver fixes while still claiming to rebuild CoachZ userspace locally.
CARGO_TARGET_DIR="$target_dir" cargo update \
    -Z unstable-options \
    --lockfile-path "$lock_file" \
    --config "$config" \
    --manifest-path "$scarlet_root/.cargo/Cargo.toml" \
    -p sgfx-backend-scarlet-adreno
CARGO_TARGET_DIR="$target_dir" cargo build \
    -Z unstable-options \
    -Z build-std=core,compiler_builtins,alloc \
    -Z build-std-features=compiler-builtins-mem \
    --lockfile-path "$lock_file" \
    --config "$config" \
    --manifest-path "$scarlet_root/.cargo/Cargo.toml" \
    --target aarch64-unknown-scarlet \
    --release \
    -p userprogram \
    --bin sgfx_probe \
    --bin taskbar \
    --bin terminal \
    --bin ui-demo \
    --bin ui-benchmark \
    --bin settings

# `userprogram` uses Scarlet's no_std compatibility layer, while std-bin uses
# the target's Rust std.  Keep them in separate Cargo feature-resolution units
# so the two runtime personalities are never unified into one backend build.
CARGO_TARGET_DIR="$target_dir" cargo build \
    -Z unstable-options \
    --lockfile-path "$lock_file" \
    --config "$config" \
    --manifest-path "$scarlet_root/.cargo/Cargo.toml" \
    --target aarch64-unknown-scarlet \
    --release \
    -p scarlet-std-bin \
    --bin sws \
    --bin clock \
    --bin files \
    --bin launcher \
    --bin notepad \
    --bin task_manager \
    --bin ui-sgfx-showcase \
    --bin sgfx_cube \
    --bin sgfx_texture \
    --bin sgfx_showcase

# The desktop bundle carries the portable Git build of Boxcraft.  When its
# sibling checkout is available, replace that binary with one resolved against
# the same ScarletUI and in-tree Adreno stack as the rest of CoachZ userspace.
# Keep the repository lock pristine: the temporary lock selects local A6xx
# packages without turning Boxcraft's portable Git dependencies into paths.
if [ -f "$boxcraft_root/Cargo.toml" ] && [ -f "$boxcraft_root/Cargo.lock" ]; then
    cp "$boxcraft_root/Cargo.lock" "$boxcraft_lock_file"
    CARGO_TARGET_DIR="$target_dir" cargo update \
        -Z unstable-options \
        --lockfile-path "$boxcraft_lock_file" \
        --config "$config" \
        --manifest-path "$boxcraft_root/Cargo.toml" \
        -p sgfx-backend-scarlet-adreno
    CARGO_TARGET_DIR="$target_dir" cargo build \
        -Z unstable-options \
        --lockfile-path "$boxcraft_lock_file" \
        --config "$config" \
        --manifest-path "$boxcraft_root/Cargo.toml" \
        --target aarch64-unknown-scarlet \
        --release \
        -p boxcraft
else
    echo "cargo-scarlet: Boxcraft checkout not found at $boxcraft_root; keeping bundled Git build" >&2
fi

for binary in \
    sgfx_probe taskbar terminal ui-demo ui-benchmark settings \
    sws clock files launcher notepad task_manager ui-sgfx-showcase \
    sgfx_cube sgfx_texture sgfx_showcase
do
    source_path="$target_bins/$binary"
    if [ ! -x "$source_path" ]; then
        echo "CoachZ userspace binary missing after build: $source_path" >&2
        exit 1
    fi
    cp "$source_path" "$destination/$binary"
done

if [ -f "$boxcraft_root/Cargo.toml" ] && [ -f "$boxcraft_root/Cargo.lock" ]; then
    source_path="$target_bins/boxcraft"
    if [ ! -x "$source_path" ]; then
        echo "CoachZ userspace binary missing after build: $source_path" >&2
        exit 1
    fi
    cp "$source_path" "$destination/boxcraft"
fi
