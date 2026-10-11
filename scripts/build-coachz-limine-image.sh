#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project_dir="$repo_root/projects/aarch64-coachz-limine"

case "${1:-}" in
  "") exec cargo scarlet image --project "$project_dir" --release ;;
  --debug)
    [[ $# -eq 1 ]] || { echo "usage: $0 [--debug]" >&2; exit 2; }
    exec cargo scarlet image --project "$project_dir"
    ;;
  *) echo "usage: $0 [--debug]" >&2; exit 2 ;;
esac
