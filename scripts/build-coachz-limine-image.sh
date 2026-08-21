#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project_dir="$repo_root/projects/aarch64-coachz-limine"
profile_flag="--release"
staging_dir="$project_dir/.scarlet/staging"

if [[ "${1:-}" == "--debug" ]]; then
  profile_flag=""
  shift
fi
if [[ $# -ne 0 ]]; then
  echo "usage: $0 [--debug]" >&2
  exit 2
fi

# cargo-scarlet leaves this generated tree behind when an image build fails.
# Starting from it again can collide with bundle-created symlinks.
if [[ -d "$staging_dir" ]]; then
  rm -rf -- "$staging_dir"
fi

if [[ -n "$profile_flag" ]]; then
  cargo scarlet image --project "$project_dir" "$profile_flag"
else
  cargo scarlet image --project "$project_dir"
fi
