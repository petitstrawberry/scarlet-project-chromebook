#!/usr/bin/env python3
"""Fetch hash-pinned CoachZ Wi-Fi firmware into the ignored local input folder.

No binary firmware is committed. Downloads and archive members are verified
before installation; build steps never fetch unpinned inputs implicitly.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile
import tempfile
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-coachz-limine"
MANIFEST = PROJECT / "wifi-firmware.json"


def digest(path, algorithm="sha256"):
    result = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def relative_path(name):
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise ValueError(f"unsafe manifest path: {name}")
    return Path(*path.parts)


def verify(path, entry):
    if not path.is_file():
        raise ValueError(f"missing firmware: {entry['path']}; run python3 scripts/fetch-wifi-firmware.py")
    if path.stat().st_size != entry["size"]:
        raise ValueError(f"wrong-size firmware: {entry['path']}")
    if digest(path) != entry["sha256"]:
        raise ValueError(f"firmware checksum mismatch: {entry['path']}")


def download(source, destination):
    url = source["url"]
    if not url.startswith("https://"):
        raise ValueError("firmware sources must use HTTPS")
    print(f"Fetching {source['name']}...", flush=True)
    request = Request(url, headers={"User-Agent": "Scarlet-CoachZ-firmware/1"})
    with urlopen(request, timeout=30) as response, destination.open("wb") as out:
        if not response.geturl().startswith("https://"):
            raise ValueError("refusing a non-HTTPS firmware redirect")
        total = 0
        for block in iter(lambda: response.read(1024 * 1024), b""):
            total += len(block)
            if total > source["max_download_bytes"]:
                raise ValueError(f"oversized firmware download: {source['name']}")
            out.write(block)


def fetch(output, archive=None, verify_only=False):
    pins = json.loads(MANIFEST.read_text())
    if pins["schema_version"] != 1:
        raise ValueError("unsupported firmware manifest")
    entries = [entry for source in pins["sources"] for entry in source["files"]]
    paths = [relative_path(entry["path"]) for entry in entries]
    if len(set(paths)) != len(paths):
        raise ValueError("duplicate firmware destination in manifest")
    # Preserve existing local inputs; never silently replace different firmware.
    for entry, path in zip(entries, paths):
        target = output / path
        if target.exists() or verify_only:
            verify(target, entry)
    if verify_only:
        print(f"Verified {len(entries)} pinned files in {output}")
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".wifi-download-", dir=output.parent) as temporary:
        work = Path(temporary)
        staged = work / "files"
        for index, source in enumerate(pins["sources"]):
            missing = [entry for entry in source["files"] if not (output / relative_path(entry["path"])).exists()]
            if not missing:
                continue
            payload = work / f"source-{index}"
            if source["kind"] == "tar.gz" and archive is not None:
                payload = archive
            else:
                download(source, payload)
            if source["kind"] == "tar.gz":
                if payload.stat().st_size > source["max_download_bytes"] or digest(payload, "sha512") != source["sha512"]:
                    raise ValueError(f"firmware archive checksum mismatch: {source['name']}")
                with tarfile.open(payload, mode="r:gz") as tar:
                    for entry in missing:
                        matches = [member for member in tar.getmembers() if member.name == entry["member"]]
                        if len(matches) != 1 or not matches[0].isfile() or matches[0].size != entry["size"]:
                            raise ValueError(f"invalid archive member: {entry['member']}")
                        target = staged / relative_path(entry["path"])
                        target.parent.mkdir(parents=True, exist_ok=True)
                        # Only explicitly pinned regular members; no extractall,
                        # symlinks, path traversal or unrelated LTE firmware.
                        with tar.extractfile(matches[0]) as src, target.open("wb") as dst:
                            shutil.copyfileobj(src, dst)
                        verify(target, entry)
            elif source["kind"] == "file" and len(missing) == 1:
                verify(payload, missing[0])
                target = staged / relative_path(missing[0]["path"])
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(payload, target)
            else:
                raise ValueError(f"unsupported firmware source: {source['name']}")
        # No target is changed until all required downloads have been verified.
        output.mkdir(parents=True, exist_ok=True)
        for entry, path in zip(entries, paths):
            target = output / path
            if target.exists():
                verify(target, entry)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            os.replace(staged / path, target)
            verify(target, entry)
        provenance = json.dumps(pins, indent=2) + "\n"
        (work / "SOURCE.json").write_text(provenance)
        os.replace(work / "SOURCE.json", output / "SOURCE.json")
    print(f"Installed and verified {len(entries)} local firmware files: {output}")


def install(output, target):
    """Build-layer entry point: verify local inputs and copy only pinned files."""
    pins = json.loads(MANIFEST.read_text())
    fetch(output, verify_only=True)
    for source in pins["sources"]:
        for entry in source["files"]:
            path = relative_path(entry["path"])
            if path.parts[0] == "licenses":
                destination = target / "usr/share/licenses/coachz-wifi" / path.name
            else:
                destination = target / "lib/firmware" / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(output / path, destination)
    provenance = target / "usr/share/licenses/coachz-wifi/SOURCE.json"
    provenance.parent.mkdir(parents=True, exist_ok=True)
    provenance.write_text(json.dumps(pins, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=PROJECT / "firmware/wifi")
    parser.add_argument("--archive", type=Path, help="reuse the pinned MSS archive (SHA-512 is still checked)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--verify", action="store_true", help="verify local files without downloading")
    mode.add_argument("--install", type=Path, metavar="ROOT", help="install verified local inputs into a build layer; never download")
    args = parser.parse_args()
    if args.install:
        if args.archive:
            parser.error("--archive is only valid when fetching")
        install(args.output, args.install)
    else:
        fetch(args.output, args.archive, args.verify)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError) as error:
        print(f"coachz-wifi-firmware: {error}", file=sys.stderr)
        sys.exit(1)
