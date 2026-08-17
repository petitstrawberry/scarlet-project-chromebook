#!/bin/sh
# Fetch a reproducible local ChromiumOS EC checkout for the USB-console helpers.
set -eu

readonly EC_URL_DEFAULT='https://chromium.googlesource.com/chromiumos/platform/ec'
readonly EC_REVISION_DEFAULT='ab6941cd8b5b973152d6ad947daa43160d8e9d2b'

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
destination=${EC_SOURCE_DIR:-"$project_root/.cache/chromiumos-ec"}
revision=${EC_REVISION:-$EC_REVISION_DEFAULT}
repository=${EC_REPOSITORY:-$EC_URL_DEFAULT}

usage() {
    cat <<'EOF'
Usage: fetch-chromiumos-ec.sh [OPTIONS]

Fetch or update a detached ChromiumOS EC source checkout. The default checkout is
.cache/chromiumos-ec and the default revision is pinned for reproducibility.

Options:
  --dest DIR          Checkout directory (default: $EC_SOURCE_DIR or .cache/chromiumos-ec)
  --revision REV      Git revision (default: $EC_REVISION or the pinned revision)
  --repository URL    ChromiumOS EC Git URL (default: $EC_REPOSITORY or upstream)
  -h, --help          Show this help text

Environment overrides: EC_SOURCE_DIR, EC_REVISION, EC_REPOSITORY.
EOF
}

die() {
    printf '%s\n' "error: $*" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dest)
            [ "$#" -ge 2 ] || die '--dest requires a directory'
            destination=$2
            shift 2
            ;;
        --revision)
            [ "$#" -ge 2 ] || die '--revision requires a revision'
            revision=$2
            shift 2
            ;;
        --repository)
            [ "$#" -ge 2 ] || die '--repository requires a URL'
            repository=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1" ;;
    esac
done

[ -n "$revision" ] || die 'revision must not be empty'
command -v git >/dev/null 2>&1 || die 'git is required'

if [ -e "$destination" ] && [ ! -d "$destination/.git" ]; then
    die "destination exists but is not a Git checkout: $destination"
fi

if [ ! -d "$destination/.git" ]; then
    mkdir -p "$(dirname -- "$destination")"
    printf 'Cloning ChromiumOS EC source into %s\n' "$destination"
    git init "$destination" >/dev/null
    git -C "$destination" remote add origin "$repository"
else
    existing_repository=$(git -C "$destination" remote get-url origin 2>/dev/null || true)
    [ -n "$existing_repository" ] || die "checkout has no origin remote: $destination"
    [ "$existing_repository" = "$repository" ] || die "origin differs from requested repository: $existing_repository"
fi

printf 'Fetching ChromiumOS EC revision %s\n' "$revision"
git -C "$destination" fetch --depth=1 origin "$revision"
# Do not discard local changes when --dest points at a user-managed checkout.
# A clean cache checkout still moves to the requested revision normally.
git -C "$destination" checkout --detach FETCH_HEAD >/dev/null

actual_revision=$(git -C "$destination" rev-parse HEAD)
printf 'ChromiumOS EC ready: %s (%s)\n' "$destination" "$actual_revision"
