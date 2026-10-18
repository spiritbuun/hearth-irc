#!/usr/bin/env python3
"""Create an explicit-file-list binary bundle; never archive runtime state."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import subprocess
import tarfile
import tempfile


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target triple; defaults to rustc's host")
    args = parser.parse_args()
    host = next(line.split(": ", 1)[1] for line in subprocess.check_output(
        ["rustc", "-vV"], text=True).splitlines() if line.startswith("host: "))
    target = args.target or host
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", target) or "linux" not in target:
        parser.error("use a Linux Rust target triple")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=root))
    package = next(p for p in metadata["packages"] if p["name"] == "hearth-irc")
    # Keep the maintainer's home/workspace paths out of distributed executables.
    environment = os.environ.copy()
    flags = (environment["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
             if environment.get("CARGO_ENCODED_RUSTFLAGS")
             else shlex.split(environment.get("RUSTFLAGS", "")))
    flags += [f"--remap-path-prefix={Path.home()}=/build/user",
              f"--remap-path-prefix={root}=."]
    environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    subprocess.run(["cargo", "build", "--release", "--locked", "--bins", "--target", target],
                   cwd=root, env=environment, check=True)
    output = root / "dist"
    output.mkdir(exist_ok=True)
    name = f"hearth-irc-{package['version']}-{target}"
    archive = output / f"{name}.tar.gz"
    with tempfile.TemporaryDirectory(prefix="hearth-package-") as temp:
        bundle = Path(temp) / name
        (bundle / "bin").mkdir(parents=True)
        (bundle / "scripts").mkdir()
        for binary in ("hearthd", "hearth-ircd", "hearth-admin"):
            shutil.copy2(Path(metadata["target_directory"]) / target / "release" / binary,
                         bundle / "bin" / binary)
        for file in (root / "README.md", *root.glob("LICENSE*")):
            if file.is_file():
                shutil.copy2(file, bundle / file.name)
        for file in ("install.sh", "install-runit.py"):
            shutil.copy2(root / "scripts" / file, bundle / "scripts" / file)
        # Explicit distributable assets, not a recursive copy of the checkout.
        for file in ("docs/services.md", "docs/runit.md", "docs/distribution.md",
                     "deploy/systemd/hearth-ircd.service", "skills/hearth/SKILL.md"):
            destination = bundle / file
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / file, destination)
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(bundle, arcname=name)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n")
    print(archive)


if __name__ == "__main__":
    main()
