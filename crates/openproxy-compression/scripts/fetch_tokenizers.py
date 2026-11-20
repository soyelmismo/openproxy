#!/usr/bin/env python3
"""Materialize the pinned public tokenizer artifacts into a local directory.

The repository never stores multi-megabyte vocabularies. This script reads the
embedded manifest (assets/tokenizers/manifest.json), downloads each artifact
from its pinned revision URL and verifies the sha256 before writing it under
the requested directory. It is the only supported way to populate
OPENPROXY_TOKENIZER_DIR for the versioned artifacts.

Usage:
    python3 scripts/fetch_tokenizers.py [--dir DIR] [--only ID ...] [--check]

--check verifies already-present files without downloading.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys
import urllib.request

MANIFEST = pathlib.Path(__file__).resolve().parent.parent / "assets" / "tokenizers" / "manifest.json"
DEFAULT_DIR = "/root/.openproxy/tokenizers"


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download(url: str, dest: pathlib.Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    print(f"GET {url}")
    with urllib.request.urlopen(url) as response, tmp.open("wb") as out:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            out.write(chunk)
    tmp.replace(dest)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dir", default=DEFAULT_DIR)
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    manifest = json.loads(MANIFEST.read_text())
    target = pathlib.Path(args.dir)
    failures = []
    for artifact in manifest.get("artifacts", []):
        if args.only and artifact["id"] not in args.only:
            continue
        dest = target / artifact["filename"]
        expected = artifact["sha256"]
        if dest.exists():
            actual = sha256(dest)
            if actual == expected:
                print(f"ok   {dest} ({actual})")
                continue
            failures.append(f"{dest}: sha256 {actual} != pinned {expected}")
            print(f"BAD  {dest}: sha256 mismatch", file=sys.stderr)
            continue
        if args.check:
            failures.append(f"{dest}: missing (pinned {expected})")
            print(f"MISS {dest}", file=sys.stderr)
            continue
        download(artifact["url"], dest)
        actual = sha256(dest)
        if actual != expected:
            failures.append(f"{dest}: downloaded sha256 {actual} != pinned {expected}")
            print(f"BAD  {dest}: downloaded hash mismatch", file=sys.stderr)
        else:
            print(f"ok   {dest} ({actual})")

    if failures:
        print("\nAsset verification failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("all pinned tokenizer artifacts verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
