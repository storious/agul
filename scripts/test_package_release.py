#!/usr/bin/env python3
from __future__ import annotations

import re
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path
from unittest import mock
from urllib.parse import unquote, urlsplit

import package_release


MARKDOWN_LINK = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
VERSION = "9.8.7"


class PackageReleaseTests(unittest.TestCase):
    def test_archives_include_documentation_and_resolve_local_links(self) -> None:
        cases = (
            ("tar.gz", "x86_64-unknown-linux-gnu", "agul"),
            ("zip", "x86_64-pc-windows-msvc", "agul.exe"),
            ("tar.gz", "x86_64-apple-darwin", "agul"),
            ("tar.gz", "aarch64-apple-darwin", "agul"),
        )
        for archive_format, target, executable in cases:
            with self.subTest(archive_format=archive_format), tempfile.TemporaryDirectory(
                prefix="agul-package-test-"
            ) as temporary:
                temporary_root = Path(temporary)
                binary = temporary_root / "fixture-binary"
                binary.write_bytes(b"fixture")
                completed = subprocess.CompletedProcess(
                    [binary, "--version"], 0, stdout=f"agul {VERSION}\n"
                )

                with mock.patch.object(
                    package_release.subprocess, "run", return_value=completed
                ):
                    archive = package_release.package_release(
                        binary=binary,
                        target=target,
                        version=f"v{VERSION}",
                        archive_format=archive_format,
                        output=temporary_root / "dist",
                    )

                extracted = temporary_root / "extracted"
                extracted.mkdir()
                if archive_format == "zip":
                    with zipfile.ZipFile(archive) as packaged:
                        packaged.extractall(extracted)
                else:
                    with tarfile.open(archive, "r:gz") as packaged:
                        packaged.extractall(extracted, filter="data")

                root = extracted / f"agul-v{VERSION}-{target}"
                self.assertTrue((root / executable).is_file())
                self.assertEqual(
                    (root / "LICENSE").read_bytes(),
                    (package_release.REPOSITORY_ROOT / "LICENSE").read_bytes(),
                )
                self.assertEqual(
                    (root / "THIRD_PARTY_NOTICES.md").read_bytes(),
                    (
                        package_release.REPOSITORY_ROOT / "THIRD_PARTY_NOTICES.md"
                    ).read_bytes(),
                )
                self.assertTrue((root / "Cargo.lock").is_file())
                self.assertTrue((root / "README.md").is_file())
                self.assertTrue((root / "CONTRIBUTING.md").is_file())
                self.assertTrue((root / "docs/assets/agul-demo.gif").is_file())
                self.assertTrue((root / "schemas/plugin-v2.schema.json").is_file())
                release_notes = (root / "docs/release.md").read_text(encoding="utf-8")
                self.assertNotIn("{{TAG}}", release_notes)
                self.assertIn(f"# Agul v{VERSION}", release_notes)
                self.assertIn(
                    f"/releases/download/v{VERSION}/agul-demo.gif", release_notes
                )
                self.assertIn(
                    f"/releases/download/v{VERSION}/install.sh", release_notes
                )
                self.assertIn(
                    f"/releases/download/v{VERSION}/install.ps1", release_notes
                )
                self.assertIn(f"/blob/v{VERSION}/docs/README.md", release_notes)
                self.assert_markdown_links_resolve(root)

    def test_notices_match_direct_runtime_versions_in_cargo_lock(self) -> None:
        repository = package_release.REPOSITORY_ROOT
        manifest = tomllib.loads((repository / "Cargo.toml").read_text(encoding="utf-8"))
        lock = tomllib.loads((repository / "Cargo.lock").read_text(encoding="utf-8"))
        notices = (repository / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")

        runtime_dependencies = dict(manifest["dependencies"])
        for target in manifest.get("target", {}).values():
            runtime_dependencies.update(target.get("dependencies", {}))

        root_package = next(
            package
            for package in lock["package"]
            if package["name"] == manifest["package"]["name"]
            and package["version"] == manifest["package"]["version"]
        )
        locked_packages = {}
        for package in lock["package"]:
            locked_packages.setdefault(package["name"], set()).add(package["version"])
        root_dependencies = {}
        for dependency in root_package["dependencies"]:
            fields = dependency.split()
            name = fields[0]
            if len(fields) > 1 and fields[1][0].isdigit():
                root_dependencies[name] = fields[1]
            else:
                versions = locked_packages[name]
                self.assertEqual(len(versions), 1, f"ambiguous locked version for {name}")
                root_dependencies[name] = next(iter(versions))

        for manifest_name, specification in runtime_dependencies.items():
            package_name = (
                specification.get("package", manifest_name)
                if isinstance(specification, dict)
                else manifest_name
            )
            version = root_dependencies[package_name]
            self.assertIn(f"| `{package_name}` | `{version}` |", notices)

        self.assertNotIn("| `httpmock` |", notices)
        self.assertNotIn("| `tempfile` |", notices)

    def assert_markdown_links_resolve(self, root: Path) -> None:
        for markdown in root.rglob("*.md"):
            contents = markdown.read_text(encoding="utf-8")
            for raw_target in MARKDOWN_LINK.findall(contents):
                target = raw_target.strip().split(maxsplit=1)[0].strip("<>")
                parsed = urlsplit(target)
                if parsed.scheme or parsed.netloc or not parsed.path:
                    continue
                linked = markdown.parent / unquote(parsed.path)
                self.assertTrue(
                    linked.exists(),
                    f"{markdown.relative_to(root)} links to missing {target}",
                )


if __name__ == "__main__":
    unittest.main()
