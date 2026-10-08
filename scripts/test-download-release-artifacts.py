#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
import warnings
import zipfile


SCRIPT = Path(__file__).with_name("download-release-artifacts.py")
PLATFORMS = (
    "yututui-linux-arm64.tar.gz",
    "yututui-linux-x64.tar.gz",
    "yututui-macos-arm64.tar.gz",
    "yututui-macos-x64.tar.gz",
    "yututui-windows-x64.zip",
)
GH_TOKEN_SENTINEL = "gh-token-must-not-be-logged"
GITHUB_TOKEN_SENTINEL = "github-token-must-not-be-logged"


class FakeGitHub:
    def __init__(self, root):
        self.root = root
        self.bin_dir = root / "bin"
        self.data_dir = root / "fake-gh"
        self.bin_dir.mkdir()
        self.data_dir.mkdir()
        self.artifacts = []
        fake_gh = self.bin_dir / "gh"
        fake_gh.write_text(
            """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FAKE_GH_ROOT"])
args = sys.argv[1:]
endpoint = args[-1] if args else ""
if "/actions/runs/" in endpoint and endpoint.endswith("/artifacts?per_page=100"):
    diagnostic = os.environ.get("FAKE_GH_LIST_DIAGNOSTIC")
    if diagnostic:
        print(
            f"{diagnostic}: {os.environ.get('GH_TOKEN', '')} "
            f"{os.environ.get('GITHUB_TOKEN', '')}",
            file=sys.stderr,
        )
    exit_status = int(os.environ.get("FAKE_GH_LIST_EXIT", "0"))
    if exit_status:
        sys.exit(exit_status)
    with (root / "metadata.json").open() as source:
        artifacts = json.load(source)
    json.dump([{"total_count": len(artifacts), "artifacts": artifacts}], sys.stdout)
    sys.exit(0)
if "/actions/artifacts/" in endpoint and endpoint.endswith("/zip"):
    diagnostic = os.environ.get("FAKE_GH_DOWNLOAD_DIAGNOSTIC")
    if diagnostic:
        print(
            f"{diagnostic}: {os.environ.get('GH_TOKEN', '')} "
            f"{os.environ.get('GITHUB_TOKEN', '')}",
            file=sys.stderr,
        )
    exit_status = int(os.environ.get("FAKE_GH_DOWNLOAD_EXIT", "0"))
    if exit_status:
        sys.exit(exit_status)
    artifact_id = endpoint.split("/actions/artifacts/", 1)[1].split("/", 1)[0]
    path = root / f"{artifact_id}.zip"
    if not path.is_file():
        sys.exit(3)
    sys.stdout.buffer.write(path.read_bytes())
    sys.exit(0)
sys.exit(4)
""",
            encoding="utf-8",
        )
        fake_gh.chmod(0o755)

    def add_artifact(
        self,
        name,
        artifact_id,
        members,
        *,
        expired=False,
        digest="auto",
        duplicate_member=None,
        symlink_member=None,
    ):
        zip_path = self.data_dir / f"{artifact_id}.zip"
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(zip_path, "w") as archive:
                for member_name, content in members.items():
                    archive.writestr(member_name, content)
                if duplicate_member is not None:
                    archive.writestr(duplicate_member, b"duplicate")
                if symlink_member is not None:
                    info = zipfile.ZipInfo(symlink_member)
                    info.create_system = 3
                    info.external_attr = (stat.S_IFLNK | 0o777) << 16
                    archive.writestr(info, b"target")

        metadata = {"name": name, "id": artifact_id, "expired": expired}
        if digest == "auto":
            metadata["digest"] = "sha256:" + hashlib.sha256(
                zip_path.read_bytes()
            ).hexdigest()
        elif digest is not None:
            metadata["digest"] = digest
        self.artifacts.append(metadata)
        return metadata

    def write_metadata(self):
        (self.data_dir / "metadata.json").write_text(
            json.dumps(self.artifacts), encoding="utf-8"
        )


class DownloadReleaseArtifactsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(
            prefix="test-download-release-artifacts-"
        )
        self.root = Path(self.temporary.name)
        self.destination = self.root / "output"
        self.destination.mkdir()
        self.github = FakeGitHub(self.root)

    def tearDown(self):
        self.temporary.cleanup()

    def run_downloader(self, mode="platforms", extra_environment=None):
        self.github.write_metadata()
        environment = os.environ.copy()
        environment.update(
            {
                "FAKE_GH_ROOT": str(self.github.data_dir),
                "GH_TOKEN": GH_TOKEN_SENTINEL,
                "GITHUB_TOKEN": GITHUB_TOKEN_SENTINEL,
                "GITHUB_REPOSITORY": "example/yututui",
                "GITHUB_RUN_ID": "12345",
                "PATH": str(self.github.bin_dir) + os.pathsep + environment["PATH"],
            }
        )
        if extra_environment:
            environment.update(extra_environment)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), mode, "--dest", str(self.destination)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
        )
        self.assertNotIn(GH_TOKEN_SENTINEL, result.stdout)
        self.assertNotIn(GH_TOKEN_SENTINEL, result.stderr)
        self.assertNotIn(GITHUB_TOKEN_SENTINEL, result.stdout)
        self.assertNotIn(GITHUB_TOKEN_SENTINEL, result.stderr)
        return result

    def add_platforms(self, digest_overrides=None):
        digest_overrides = digest_overrides or {}
        expected = {}
        for artifact_id, name in enumerate(PLATFORMS, start=1):
            members = {
                name: f"archive:{name}".encode(),
                f"{name}.sha256": f"digest  {name}\n".encode(),
            }
            self.github.add_artifact(
                name,
                artifact_id,
                members,
                digest=digest_overrides.get(name, "auto"),
            )
            expected.update(members)
        return expected

    def assert_clean_failure(self, result, message):
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_platform_mode_downloads_flat_verified_files(self):
        expected = self.add_platforms()
        result = self.run_downloader()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            sorted(path.name for path in self.destination.iterdir()), sorted(expected)
        )
        for name, content in expected.items():
            self.assertEqual((self.destination / name).read_bytes(), content)

    def test_checksums_mode_selects_only_checksums_artifact(self):
        self.github.add_artifact(
            PLATFORMS[0],
            1,
            {PLATFORMS[0]: b"ignored", f"{PLATFORMS[0]}.sha256": b"ignored"},
        )
        self.github.add_artifact("checksums", 2, {"checksums.txt": b"sums\n"})
        result = self.run_downloader("checksums")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [path.name for path in self.destination.iterdir()], ["checksums.txt"]
        )
        self.assertEqual((self.destination / "checksums.txt").read_bytes(), b"sums\n")

    def test_gh_success_diagnostics_are_preserved_with_tokens_redacted(self):
        self.github.add_artifact("checksums", 1, {"checksums.txt": b"sums\n"})
        result = self.run_downloader(
            "checksums",
            {
                "FAKE_GH_LIST_DIAGNOSTIC": "list warning remains",
                "FAKE_GH_DOWNLOAD_DIAGNOSTIC": "download warning remains",
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("list warning remains: [REDACTED] [REDACTED]", result.stderr)
        self.assertIn("download warning remains: [REDACTED] [REDACTED]", result.stderr)

    def test_gh_listing_failure_diagnostic_is_preserved_with_tokens_redacted(self):
        result = self.run_downloader(
            "checksums",
            {
                "FAKE_GH_LIST_DIAGNOSTIC": "list error remains",
                "FAKE_GH_LIST_EXIT": "23",
            },
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("list error remains: [REDACTED] [REDACTED]", result.stderr)
        self.assertIn("gh api failed with exit status 23", result.stderr)
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_gh_download_failure_diagnostic_is_preserved_with_tokens_redacted(self):
        self.github.add_artifact("checksums", 1, {"checksums.txt": b"sums\n"})
        result = self.run_downloader(
            "checksums",
            {
                "FAKE_GH_DOWNLOAD_DIAGNOSTIC": "download error remains",
                "FAKE_GH_DOWNLOAD_EXIT": "24",
            },
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download error remains: [REDACTED] [REDACTED]", result.stderr)
        self.assertIn("gh api failed with exit status 24", result.stderr)
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_missing_duplicate_and_expired_artifacts_fail(self):
        cases = ("missing", "duplicate", "expired")
        for case in cases:
            with self.subTest(case=case):
                with tempfile.TemporaryDirectory(prefix=f"artifact-{case}-") as temporary:
                    root = Path(temporary)
                    self.destination = root / "output"
                    self.destination.mkdir()
                    self.github = FakeGitHub(root)
                    self.add_platforms()
                    if case == "missing":
                        self.github.artifacts = [
                            artifact
                            for artifact in self.github.artifacts
                            if artifact["name"] != PLATFORMS[-1]
                        ]
                        message = "missing artifact"
                    elif case == "duplicate":
                        name = PLATFORMS[-1]
                        self.github.add_artifact(
                            name,
                            20,
                            {name: b"archive", f"{name}.sha256": b"sidecar"},
                        )
                        message = "duplicate nonexpired artifact"
                    else:
                        self.github.artifacts[-1]["expired"] = True
                        message = "is expired"
                    self.assert_clean_failure(self.run_downloader(), message)

    def test_missing_and_bad_digests_fail(self):
        cases = ((None, "lacks a valid sha256 digest"), ("sha256:bad", "lacks a valid"))
        for digest, message in cases:
            with self.subTest(digest=digest):
                with tempfile.TemporaryDirectory(prefix="artifact-digest-") as temporary:
                    root = Path(temporary)
                    self.destination = root / "output"
                    self.destination.mkdir()
                    self.github = FakeGitHub(root)
                    self.add_platforms({PLATFORMS[0]: digest})
                    self.assert_clean_failure(self.run_downloader(), message)

    def test_digest_mismatch_after_other_downloads_writes_nothing(self):
        self.add_platforms({PLATFORMS[-1]: "sha256:" + "0" * 64})
        result = self.run_downloader()
        self.assert_clean_failure(result, "failed sha256 verification")

    def test_unsafe_extra_duplicate_and_symlink_zip_members_fail(self):
        cases = ("unsafe", "extra", "duplicate", "symlink")
        for case in cases:
            with self.subTest(case=case):
                with tempfile.TemporaryDirectory(prefix=f"artifact-zip-{case}-") as temporary:
                    root = Path(temporary)
                    self.destination = root / "output"
                    self.destination.mkdir()
                    self.github = FakeGitHub(root)
                    members = {"checksums.txt": b"sums\n"}
                    options = {}
                    if case == "unsafe":
                        members = {"../checksums.txt": b"sums\n"}
                        message = "unsafe ZIP member"
                    elif case == "extra":
                        members["extra.txt"] = b"extra"
                        message = "unexpected"
                    elif case == "duplicate":
                        options["duplicate_member"] = "checksums.txt"
                        message = "duplicate ZIP members"
                    else:
                        members = {}
                        options["symlink_member"] = "checksums.txt"
                        message = "non-file ZIP member"
                    self.github.add_artifact("checksums", 1, members, **options)
                    self.assert_clean_failure(
                        self.run_downloader("checksums"), message
                    )

    def test_preexisting_output_is_not_clobbered(self):
        self.add_platforms()
        existing = self.destination / PLATFORMS[0]
        existing.write_bytes(b"keep")
        result = self.run_downloader()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to overwrite", result.stderr)
        self.assertEqual(existing.read_bytes(), b"keep")
        self.assertEqual([path.name for path in self.destination.iterdir()], [PLATFORMS[0]])


if __name__ == "__main__":
    unittest.main()
