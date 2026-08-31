#!/usr/bin/env python3
from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
RELEASE_FILES = (
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "Cargo.lock",
    "README.md",
    "CONTRIBUTING.md",
)
RELEASE_DIRECTORIES = ("docs", "schemas")


def package_release(
    *,
    binary: Path,
    target: str,
    version: str,
    archive_format: str,
    output: Path,
    repository_root: Path = REPOSITORY_ROOT,
) -> Path:
    binary = binary.resolve(strict=True)
    version = version.removeprefix("v")
    root_name = f"agul-v{version}-{target}"
    executable = "agul.exe" if target.endswith("windows-msvc") else "agul"
    suffix = ".zip" if archive_format == "zip" else ".tar.gz"
    destination = output / f"{root_name}{suffix}"

    reported = subprocess.run(
        [binary, "--version"], check=True, capture_output=True, text=True
    ).stdout.strip()
    if reported != f"agul {version}":
        raise SystemExit(f"binary reported {reported!r}, expected 'agul {version}'")

    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="agul-release-") as temporary:
        root = Path(temporary) / root_name
        root.mkdir()
        shutil.copy2(binary, root / executable)
        for name in RELEASE_FILES:
            shutil.copy2(repository_root / name, root / name)
        for name in RELEASE_DIRECTORIES:
            shutil.copytree(repository_root / name, root / name)

        release_notes = root / "docs" / "release.md"
        release_notes.write_bytes(
            release_notes.read_bytes().replace(b"{{TAG}}", f"v{version}".encode())
        )

        if archive_format == "zip":
            with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(root.rglob("*")):
                    if path.is_file():
                        archive.write(path, path.relative_to(root.parent).as_posix())
        else:
            with tarfile.open(destination, "w:gz") as archive:
                archive.add(root, arcname=root_name)
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description="Package one Agul release binary.")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--format", choices=("tar.gz", "zip"), required=True)
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()

    destination = package_release(
        binary=args.binary,
        target=args.target,
        version=args.version,
        archive_format=args.format,
        output=args.output,
    )
    print(destination)


if __name__ == "__main__":
    main()
