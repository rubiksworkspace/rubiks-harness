#!/usr/bin/env python3
"""Stage a Prime Agent release layout around a given binary, for running the
harness inside benchmark containers.

The layout is exactly the release packager's (scripts/package_release.py:
binary, package.json, bundled catalog, prime-agent-runtime, skills, docs),
reusing its staging, validation and version pin. The packager itself insists
on the glibc release build plus its split debug decoder on Linux; benchmark
images span many distributions, so this stages the static musl build instead.

Usage:
  stage_harness.py --binary target/x86_64-unknown-linux-musl/release/prime-agent \
      --catalog-assets target/catalog-assets --out target/bench-harness
"""
import argparse
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "scripts" / "release"))

import package_release  # noqa: E402


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--catalog-assets", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    version = package_release.workspace_version(ROOT)
    binary = args.binary.resolve()
    catalog = package_release.resolve_catalog_assets(
        argparse.Namespace(catalog_assets=args.catalog_assets.resolve())
    )
    out = args.out.resolve()
    if out.exists():
        shutil.rmtree(out)
    package_release.stage(ROOT, binary, version, out, catalog, "prime-agent")
    package_release.validate(out, version, "prime-agent")
    package_release.pin_version(binary, out, version, "prime-agent")
    print(f"staged prime-agent {version} at {out}")


if __name__ == "__main__":
    main()
