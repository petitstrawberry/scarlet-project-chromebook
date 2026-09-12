#!/bin/sh
# Check the production driver against a coordinated Scarlet kernel checkout.
# Requires a nightly Rust toolchain and its aarch64-unknown-none target.
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 /path/to/Scarlet" >&2
    exit 2
fi

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scarlet_source=$(CDPATH= cd -- "$1" && pwd)
integration_dir="$project_root/target/a618-kernel-integration"
a618_cargo=${A618_CARGO:-cargo}
mkdir -p "$integration_dir"

python3 - "$scarlet_source" "$integration_dir/config.toml" <<'PY'
import json
from pathlib import Path
import sys

kernel = Path(sys.argv[1]) / "kernel"
if not (kernel / "Cargo.toml").is_file():
    raise SystemExit(f"missing Scarlet kernel manifest: {kernel / 'Cargo.toml'}")
Path(sys.argv[2]).write_text(
    '[patch."https://github.com/petitstrawberry/Scarlet"]\n'
    f'scarlet = {{ path = {json.dumps(str(kernel))} }}\n'
)
PY

cp "$project_root/drivers/gpu/qcom-adreno-a618/Cargo.lock" "$integration_dir/Cargo.lock"
cd "$project_root"
# A version change in the selected checkout must not leave an older git package
# locked in place and silently bypass the path patch. Only update the test lock.
"$a618_cargo" update -Z unstable-options \
    --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml \
    --config "$integration_dir/config.toml" \
    --lockfile-path "$integration_dir/Cargo.lock" -p scarlet
"$a618_cargo" check -Z unstable-options \
    --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml \
    --config "$integration_dir/config.toml" \
    --lockfile-path "$integration_dir/Cargo.lock" \
    --target aarch64-unknown-none --features scarlet/network
"$a618_cargo" tree -Z unstable-options --locked \
    --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml \
    --config "$integration_dir/config.toml" \
    --lockfile-path "$integration_dir/Cargo.lock" \
    --target aarch64-unknown-none --features scarlet/network --invert scarlet
"$a618_cargo" check -Z unstable-options --locked --release \
    --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml \
    --config "$integration_dir/config.toml" \
    --lockfile-path "$integration_dir/Cargo.lock" \
    --target aarch64-unknown-none --features scarlet/network,strict-command-validation
