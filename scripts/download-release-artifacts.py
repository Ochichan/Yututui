#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import zipfile


PLATFORM_ARTIFACTS = (
    "yututui-linux-arm64.tar.gz",
    "yututui-linux-x64.tar.gz",
    "yututui-macos-arm64.tar.gz",
    "yututui-macos-x64.tar.gz",
    "yututui-windows-x64.zip",
)
DIGEST_PATTERN = re.compile(r"sha256:([0-9a-fA-F]{64})")


class DownloadError(Exception):
    pass


def emit_gh_stderr(stderr):
    if not stderr:
        return
    redacted = stderr
    tokens = {
        value
        for name in ("GH_TOKEN", "GITHUB_TOKEN")
        if (value := os.environ.get(name))
    }
    for token in sorted(tokens, key=len, reverse=True):
        redacted = redacted.replace(token, "[REDACTED]")
    sys.stderr.write(redacted)


def expected_artifacts(mode):
    if mode == "checksums":
        return {"checksums": ("checksums.txt",)}
    return {
        name: (name, f"{name}.sha256")
        for name in PLATFORM_ARTIFACTS
    }


def run_gh_json(endpoint):
    command = ["gh", "api", "--paginate", "--slurp", endpoint]
    try:
        result = subprocess.run(
            command,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError as error:
        raise DownloadError("gh CLI is required") from error
    emit_gh_stderr(result.stderr)
    if result.returncode != 0:
        raise DownloadError(
            f"gh api failed with exit status {result.returncode} while listing artifacts"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise DownloadError("gh api returned invalid artifact metadata") from error
    if not isinstance(payload, list):
        raise DownloadError("gh api returned an unexpected artifact metadata shape")
    return payload


def list_run_artifacts(repository, run_id):
    endpoint = f"repos/{repository}/actions/runs/{run_id}/artifacts?per_page=100"
    pages = run_gh_json(endpoint)
    artifacts = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get("artifacts"), list):
            raise DownloadError("gh api returned an unexpected artifact metadata shape")
        artifacts.extend(page["artifacts"])
    return artifacts


def select_artifacts(artifacts, expected):
    selected = []
    for expected_name in expected:
        named = [
            artifact
            for artifact in artifacts
            if isinstance(artifact, dict) and artifact.get("name") == expected_name
        ]
        if not named:
            raise DownloadError(f"missing artifact {expected_name!r}")
        if any(not isinstance(artifact.get("expired"), bool) for artifact in named):
            raise DownloadError(f"artifact {expected_name!r} has invalid expiry metadata")

        active = [artifact for artifact in named if not artifact["expired"]]
        if not active:
            raise DownloadError(f"artifact {expected_name!r} is expired")
        if len(active) != 1:
            raise DownloadError(f"duplicate nonexpired artifact {expected_name!r}")

        artifact = active[0]
        artifact_id = artifact.get("id")
        if isinstance(artifact_id, bool) or not isinstance(artifact_id, int) or artifact_id <= 0:
            raise DownloadError(f"artifact {expected_name!r} has an invalid id")
        digest = artifact.get("digest")
        match = DIGEST_PATTERN.fullmatch(digest) if isinstance(digest, str) else None
        if match is None:
            raise DownloadError(f"artifact {expected_name!r} lacks a valid sha256 digest")
        selected.append((expected_name, artifact_id, match.group(1).lower()))
    return selected


def download_artifact(repository, artifact_id, destination):
    endpoint = f"repos/{repository}/actions/artifacts/{artifact_id}/zip"
    try:
        with destination.open("wb") as output:
            result = subprocess.run(
                ["gh", "api", endpoint],
                check=False,
                stdout=output,
                stderr=subprocess.PIPE,
                text=True,
            )
    except FileNotFoundError as error:
        raise DownloadError("gh CLI is required") from error
    emit_gh_stderr(result.stderr)
    if result.returncode != 0:
        raise DownloadError(
            f"gh api failed with exit status {result.returncode} "
            f"while downloading artifact id {artifact_id}"
        )


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_member_path(name, artifact_name):
    if (
        not name
        or name.startswith("/")
        or "/" in name
        or "\\" in name
        or name in {".", ".."}
    ):
        raise DownloadError(
            f"artifact {artifact_name!r} contains unsafe ZIP member {name!r}"
        )


def stage_zip_members(zip_path, artifact_name, expected_members, staging_dir):
    try:
        archive = zipfile.ZipFile(zip_path)
    except (OSError, zipfile.BadZipFile) as error:
        raise DownloadError(f"artifact {artifact_name!r} is not a valid ZIP file") from error

    with archive:
        infos = archive.infolist()
        names = [info.filename for info in infos]
        if len(names) != len(set(names)):
            raise DownloadError(f"artifact {artifact_name!r} has duplicate ZIP members")

        for info in infos:
            validate_member_path(info.filename, artifact_name)
            mode = info.external_attr >> 16
            file_type = stat.S_IFMT(mode)
            if info.is_dir() or file_type not in (0, stat.S_IFREG):
                raise DownloadError(
                    f"artifact {artifact_name!r} contains a non-file ZIP member "
                    f"{info.filename!r}"
                )

        actual = set(names)
        required = set(expected_members)
        if actual != required:
            missing = sorted(required - actual)
            extra = sorted(actual - required)
            detail = []
            if missing:
                detail.append(f"missing {missing!r}")
            if extra:
                detail.append(f"unexpected {extra!r}")
            raise DownloadError(
                f"artifact {artifact_name!r} has an invalid ZIP inventory: "
                + "; ".join(detail)
            )

        for member_name in expected_members:
            target = staging_dir / member_name
            try:
                with archive.open(member_name, "r") as source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
            except (OSError, RuntimeError, zipfile.BadZipFile) as error:
                raise DownloadError(
                    f"could not read {member_name!r} from artifact {artifact_name!r}"
                ) from error


def write_outputs(staging_dir, destination, output_names):
    existing = [name for name in output_names if (destination / name).exists()]
    if existing:
        raise DownloadError(f"refusing to overwrite existing output {existing[0]!r}")

    created = []
    try:
        for name in output_names:
            target = destination / name
            with (staging_dir / name).open("rb") as source, target.open("xb") as output:
                created.append(target)
                shutil.copyfileobj(source, output)
    except OSError as error:
        for target in created:
            try:
                target.unlink()
            except FileNotFoundError:
                pass
        raise DownloadError(f"could not write artifact output: {error}") from error


def download_release_artifacts(mode, destination, repository, run_id):
    if not destination.is_dir():
        raise DownloadError(f"destination is not a directory: {destination}")

    expected = expected_artifacts(mode)
    output_names = [name for names in expected.values() for name in names]
    existing = [name for name in output_names if (destination / name).exists()]
    if existing:
        raise DownloadError(f"refusing to overwrite existing output {existing[0]!r}")

    artifacts = list_run_artifacts(repository, run_id)
    selected = select_artifacts(artifacts, expected)

    with tempfile.TemporaryDirectory(prefix="yututui-artifacts-") as temporary:
        temporary_path = Path(temporary)
        downloads = temporary_path / "downloads"
        staging = temporary_path / "staging"
        downloads.mkdir()
        staging.mkdir()

        for artifact_name, artifact_id, expected_digest in selected:
            zip_path = downloads / f"{artifact_id}.zip"
            download_artifact(repository, artifact_id, zip_path)
            if sha256_file(zip_path) != expected_digest:
                raise DownloadError(f"artifact {artifact_name!r} failed sha256 verification")
            stage_zip_members(zip_path, artifact_name, expected[artifact_name], staging)

        write_outputs(staging, destination, output_names)
    return len(output_names)


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="Download and verify this run's release artifacts"
    )
    parser.add_argument("mode", choices=("platforms", "checksums"))
    parser.add_argument("--dest", required=True, type=Path)
    return parser.parse_args(argv)


def required_environment(name):
    value = os.environ.get(name)
    if not value:
        raise DownloadError(f"{name} is required")
    return value


def main(argv=None):
    args = parse_args(argv)
    try:
        repository = required_environment("GITHUB_REPOSITORY")
        run_id = required_environment("GITHUB_RUN_ID")
        count = download_release_artifacts(
            args.mode, args.dest, repository, run_id
        )
    except DownloadError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"downloaded {count} verified files to {args.dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
