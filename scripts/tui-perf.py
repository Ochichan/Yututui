#!/usr/bin/env python3
"""Dependency-free orchestration/reporting helpers for the ytt TUI perf matrix.

The Rust examples own native process and render measurements. This script owns the portable
parts: deterministic silence fixtures, a Range-capable constrained HTTP server, scenario-file
validation, paired fixed-seed bootstrap statistics, and merged JSON/Markdown reports.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.client
import http.server
import ipaddress
import io
import json
import math
import os
import platform
import random
import re
import secrets
import shlex
import signal
import shutil
import stat
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from pathlib import Path
from typing import Any, Iterable
from urllib.parse import urlsplit


SCHEMA = "ytt.tui-perf.report.v1"
DEFAULT_SCENARIOS = Path(__file__).with_name("tui-perf-scenarios.json")
RATIO_INFINITY = 1e300
CPU_ACCOUNTING_METHOD = "time_weighted_counter_deltas_clamped_to_measure_window"
REQUIRED_PLAYBACK_MPV_CACHE_ARGS = {
    "--demuxer-max-bytes": "32MiB",
    "--demuxer-max-back-bytes": "8MiB",
}
BUILD_RECEIPT_SCHEMA = "ytt.tui-perf.build.v1"
SEED_CONTRACT_SCHEMA = "ytt.tui-perf.seed-contract.v1"
RUN_CONTRACT_SCHEMA = "ytt.tui-perf.run-contract.v1"
HOST_IDENTITY_FIELDS = (
    "system",
    "release",
    "machine",
    "node_fingerprint",
    "machine_id_fingerprint",
    "boot_id_fingerprint",
)
SAMPLED_TREE_LIMITATION = (
    "sampled process tree; sub-interval descendants may be missed"
)
RENDER_MEASUREMENT_LIMITATION = (
    "in-process TestBackend render microbenchmark; excludes terminal I/O and OS compositor"
)
CLEANUP_SCOPE = "dedicated_owner_process_group_and_observed_exact_descendants"
CLEANUP_SCOPE_LIMITATION = (
    "process cleanup proof is limited to the dedicated owner process group and observed "
    "exact descendants; a malicious unobserved double-fork/reparent outside portable "
    "process-group containment may escape"
)
CONTROLLED_BUILD_ENV_ALLOWLIST = (
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
)
HTTP_THROTTLE_CHUNK_BYTES = 4 * 1024
HTTP_PACING_EARLY_TOLERANCE_NS = 10_000_000
HTTP_MEANINGFUL_GET_BYTES = 64 * 1024
HTTP_SHUTDOWN_SCHEMA = "ytt.tui-perf.http-shutdown.v1"
HTTP_SHUTDOWN_PATH = "/__ytt_tui_perf_shutdown__"
HTTP_SHUTDOWN_METHOD = "authenticated_loopback_http_v1"
MAX_BOOTSTRAP_RESAMPLES = 1_000_000
MAX_STATISTICS_SEED = (1 << 64) - 1
CONTROL_ACTION_SCHEDULE_LATE_TOLERANCE_NS = 100_000_000
CONTROL_PAUSE_HOLD_LATE_TOLERANCE_NS = 100_000_000
CONTROL_MIN_RESUME_PROGRESS_S = 0.01
TREE_DIGEST_DOMAIN = b"ytt.tui-perf.tree-digest.v2\0"
TREE_REGULAR_FILE_ENTRY_TAG = b"\x01regular-file\0"
EFFECTIVE_WORKTREE_DIGEST_DOMAIN = b"ytt.tui-perf.effective-worktree-digest.v2\0"
EFFECTIVE_WORKTREE_REGULAR_ENTRY_TAG = b"\x01regular-file\0"
EFFECTIVE_WORKTREE_SYMLINK_ENTRY_TAG = b"\x02symbolic-link\0"
LAUNCH_POLICY_FIELDS = (
    ("tools", "ytdlp_managed"),
    ("update_check_enabled",),
    ("media_controls",),
    ("autoplay_on_start",),
    ("autoplay_streaming",),
    ("album_art",),
    ("romanized_titles",),
    ("ai_enabled",),
    ("scrobble", "lastfm", "enabled"),
    ("scrobble", "listenbrainz", "enabled"),
    ("scrobble", "local_files"),
)
LAUNCH_POLICY_EFFECTIVE = {
    "tools.ytdlp_managed": False,
    "update_check_enabled": False,
    "media_controls": False,
    "autoplay_on_start": False,
    "autoplay_streaming": False,
    "album_art": False,
    "romanized_titles": False,
    "ai_enabled": False,
    "scrobble.lastfm.enabled": False,
    "scrobble.listenbrainz.enabled": False,
    "scrobble.local_files": False,
    "api_cookie_auth": "disabled_by_credential_absence",
    "lyrics_fetch": "closed_nonpersistent_default_with_controlled_input",
    "child_environment": "env_i_explicit_allowlist",
    "ytm_perf_enabled": False,
    "external_background_network": "disabled",
}
CHILD_ENVIRONMENT_POLICY = {
    "inheritance": "empty_env_i",
    "host_passthrough": ["PATH", "LANG", "LC_ALL", "LC_CTYPE"],
    "isolated_keys": [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "YTM_CONFIG_DIR",
        "YTM_DATA_DIR",
        "YTM_CACHE_DIR",
        "TMPDIR",
        "TEMP",
        "TMP",
        "TERM",
        "YTM_MPV_EXTRA",
        "TUI_PERF_SCENARIO_SHA256",
        "TUI_PERF_RUN_ID",
    ],
    "ambient_behavior_keys_blocked": ["GEMINI_API_KEY", "YTM_PLAY_URL", "YTM_PERF"],
}


class DuplicateJsonKeyError(ValueError):
    pass


def measurement_limitations(render: bool) -> list[str]:
    if render:
        return [RENDER_MEASUREMENT_LIMITATION]
    return [SAMPLED_TREE_LIMITATION, CLEANUP_SCOPE_LIMITATION]


def reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise DuplicateJsonKeyError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular_tree_files(root: Path) -> tuple[Path, list[Path]]:
    try:
        root_metadata = root.lstat()
    except OSError as error:
        raise ValueError(f"cannot inspect tree root {root}: {error}") from error
    is_junction = bool(getattr(root, "is_junction", lambda: False)())
    if root.is_symlink() or is_junction or not stat.S_ISDIR(root_metadata.st_mode):
        raise ValueError(f"tree root must be a real directory: {root}")
    root = root.resolve()
    files: list[Path] = []
    for path in root.rglob("*"):
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ValueError(f"cannot inspect tree entry {path}: {error}") from error
        is_junction = bool(getattr(path, "is_junction", lambda: False)())
        if path.is_symlink() or is_junction:
            raise ValueError(f"tree contains a link/reparse entry: {path}")
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError(f"tree contains a non-regular entry: {path}")
        files.append(path)
    files.sort(key=lambda path: path.relative_to(root).as_posix())
    return root, files


def update_tree_digest(digest: Any, root: Path, path: Path) -> None:
    relative = path.relative_to(root).as_posix().encode("utf-8")
    with path.open("rb") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError(f"tree entry changed to a non-regular file: {path}")
        content_length = metadata.st_size
        if len(relative) >= 1 << 64 or content_length >= 1 << 64:
            raise ValueError(f"tree entry is too large to encode: {path}")
        digest.update(TREE_REGULAR_FILE_ENTRY_TAG)
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(content_length.to_bytes(8, "big"))
        bytes_read = 0
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            bytes_read += len(chunk)
            digest.update(chunk)
        if bytes_read != content_length:
            raise ValueError(f"tree entry changed length while hashing: {path}")


def sha256_tree(root: Path) -> str:
    root, files = regular_tree_files(root)
    digest = hashlib.sha256()
    digest.update(TREE_DIGEST_DOMAIN)
    for path in files:
        update_tree_digest(digest, root, path)
    return digest.hexdigest()


def tree_file_inventory(root: Path) -> list[dict[str, Any]]:
    root, files = regular_tree_files(root)
    return [
        {
            "path": path.relative_to(root).as_posix(),
            "bytes": path.stat().st_size,
            "sha256": sha256_file(path),
        }
        for path in files
    ]


def tree_digest_self_test() -> None:
    def legacy_unframed_digest(root: Path) -> str:
        root, files = regular_tree_files(root)
        digest = hashlib.sha256()
        for path in files:
            relative = path.relative_to(root).as_posix().encode("utf-8")
            digest.update(len(relative).to_bytes(8, "big"))
            digest.update(relative)
            digest.update(path.read_bytes())
        return digest.hexdigest()

    with tempfile.TemporaryDirectory(prefix="ytt-perf-tree-digest-self-test-") as temporary:
        base = Path(temporary)
        one_file = base / "one-file"
        two_files = base / "two-files"
        empty_overlay = base / "empty-overlay"
        one_file.mkdir()
        two_files.mkdir()
        empty_overlay.mkdir()
        prefix = b"first-content"
        suffix = b"second-content"
        encoded_second_entry = len(b"b").to_bytes(8, "big") + b"b" + suffix
        (one_file / "a").write_bytes(prefix + encoded_second_entry)
        (two_files / "a").write_bytes(prefix)
        (two_files / "b").write_bytes(suffix)
        if legacy_unframed_digest(one_file) != legacy_unframed_digest(two_files):
            raise AssertionError("tree collision fixture does not reproduce legacy ambiguity")
        if sha256_tree(one_file) == sha256_tree(two_files):
            raise AssertionError("length-framed tree digest retained a legacy collision")
        overlay_digest, _inventory = overlay_tree_identity(one_file, empty_overlay, [])
        if overlay_digest != sha256_tree(one_file):
            raise AssertionError("overlay and direct tree digests use different framing")

        try:
            sha256_tree(one_file / "a")
        except ValueError:
            pass
        else:
            raise AssertionError("tree digest accepted a non-directory tree root")

        link = one_file / "link"
        try:
            link.symlink_to(one_file / "a")
        except OSError:
            pass
        else:
            try:
                sha256_tree(one_file)
            except ValueError:
                pass
            else:
                raise AssertionError("tree digest accepted a symbolic link")
            link.unlink()

        if hasattr(os, "mkfifo"):
            fifo = one_file / "fifo"
            os.mkfifo(fifo)
            try:
                sha256_tree(one_file)
            except ValueError:
                pass
            else:
                raise AssertionError("tree digest accepted a non-regular entry")


def identity_for_file(path: Path) -> dict[str, Any]:
    path = path.resolve()
    if not path.is_file():
        raise ValueError(f"file does not exist: {path}")
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha256_file(path)}


def checked_identity_command(command: list[str], label: str) -> str:
    try:
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ValueError(f"cannot determine {label}: {error}") from error
    value = result.stdout.strip()
    if result.returncode != 0 or not value:
        detail = result.stderr.strip() or f"exit code {result.returncode}"
        raise ValueError(f"cannot determine {label}: {detail}")
    return value


def stable_machine_id(system: str) -> str:
    if system == "Linux":
        path = Path("/etc/machine-id")
        try:
            value = path.read_text(encoding="ascii").strip().lower()
        except OSError as error:
            raise ValueError(f"cannot determine Linux machine ID: {error}") from error
        if not re.fullmatch(r"[0-9a-f]{32}", value) or value == "0" * 32:
            raise ValueError("Linux machine ID is missing or malformed")
        return f"linux-machine-id:{value}"
    if system == "Darwin":
        output = checked_identity_command(
            ["/usr/sbin/ioreg", "-rd1", "-c", "IOPlatformExpertDevice"],
            "macOS IOPlatformUUID",
        )
        match = re.search(r'"IOPlatformUUID"\s*=\s*"([0-9A-Fa-f-]{36})"', output)
        if match is None:
            raise ValueError("macOS IOPlatformUUID is missing or malformed")
        return f"darwin-platform-uuid:{match.group(1).lower()}"
    if system == "Windows":
        value = checked_identity_command(
            [
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-CimInstance Win32_ComputerSystemProduct).UUID",
            ],
            "Windows system UUID",
        ).lower()
        if not re.fullmatch(r"[0-9a-f-]{36}", value) or value == "00000000-0000-0000-0000-000000000000":
            raise ValueError("Windows system UUID is missing or malformed")
        return f"windows-system-uuid:{value}"
    raise ValueError(f"unsupported host system for stable machine identity: {system!r}")


def stable_boot_id(system: str) -> str:
    if system == "Linux":
        path = Path("/proc/sys/kernel/random/boot_id")
        try:
            value = path.read_text(encoding="ascii").strip().lower()
        except OSError as error:
            raise ValueError(f"cannot determine Linux boot ID: {error}") from error
        if not re.fullmatch(r"[0-9a-f-]{36}", value):
            raise ValueError("Linux boot ID is missing or malformed")
        return f"linux-boot-id:{value}"
    if system == "Darwin":
        output = checked_identity_command(
            ["/usr/sbin/sysctl", "-n", "kern.boottime"], "macOS boot time"
        )
        match = re.search(r"sec\s*=\s*(\d+)\s*,\s*usec\s*=\s*(\d+)", output)
        if match is None:
            raise ValueError("macOS boot time is missing or malformed")
        return f"darwin-boottime:{int(match.group(1))}:{int(match.group(2))}"
    if system == "Windows":
        value = checked_identity_command(
            [
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks",
            ],
            "Windows boot time",
        )
        if not value.isdecimal() or int(value) <= 0:
            raise ValueError("Windows boot time is missing or malformed")
        return f"windows-boot-ticks:{value}"
    raise ValueError(f"unsupported host system for stable boot identity: {system!r}")


def host_identifier_fingerprint(label: str, raw: str) -> str:
    if not raw:
        raise ValueError(f"cannot fingerprint an empty host {label}")
    digest = hashlib.sha256()
    digest.update(b"ytt.tui-perf.host.v1\0")
    digest.update(label.encode("ascii"))
    digest.update(b"\0")
    digest.update(raw.encode("utf-8"))
    return f"sha256:{digest.hexdigest()}"


def stable_host_identity() -> dict[str, str]:
    system = platform.system()
    release = platform.release()
    machine = platform.machine()
    node = platform.node()
    identity = {
        "system": system,
        "release": release,
        "machine": machine,
    }
    if any(not isinstance(value, str) or not value.strip() for value in (*identity.values(), node)):
        raise ValueError("host system/release/machine/node identity is incomplete")
    identity["node_fingerprint"] = host_identifier_fingerprint("node", node)
    identity["machine_id_fingerprint"] = host_identifier_fingerprint(
        "machine_id", stable_machine_id(system)
    )
    identity["boot_id_fingerprint"] = host_identifier_fingerprint(
        "boot_id", stable_boot_id(system)
    )
    return identity


def run_git(root: Path, *arguments: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", "-C", str(root), *arguments],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"git {' '.join(arguments)} failed in {root}: {detail}")
    if binary:
        return result.stdout
    return result.stdout.decode("utf-8", errors="strict").strip()


def effective_worktree_digest(root: Path) -> tuple[str, int]:
    try:
        root_metadata = root.lstat()
    except OSError as error:
        raise ValueError(f"cannot inspect source root {root}: {error}") from error
    is_junction = bool(getattr(root, "is_junction", lambda: False)())
    if root.is_symlink() or is_junction or not stat.S_ISDIR(root_metadata.st_mode):
        raise ValueError(f"source root must be a real directory: {root}")
    root = root.resolve()
    raw = run_git(root, "ls-files", "-co", "--exclude-standard", "-z", binary=True)
    assert isinstance(raw, bytes)
    relative_paths = sorted(
        (item.decode("utf-8", errors="surrogateescape") for item in raw.split(b"\0") if item),
        key=lambda item: item.encode("utf-8", errors="surrogateescape"),
    )
    if len(relative_paths) != len(set(relative_paths)):
        raise ValueError(f"git returned duplicate effective worktree paths for {root}")
    digest = hashlib.sha256()
    digest.update(EFFECTIVE_WORKTREE_DIGEST_DOMAIN)

    def frame_prefix(tag: bytes, encoded_path: bytes, payload_length: int) -> None:
        if len(encoded_path) >= 1 << 64 or payload_length < 0 or payload_length >= 1 << 64:
            raise ValueError("effective worktree entry exceeds the digest framing range")
        digest.update(tag)
        digest.update(len(encoded_path).to_bytes(8, "big"))
        digest.update(encoded_path)
        digest.update(payload_length.to_bytes(8, "big"))

    def metadata_signature(metadata: os.stat_result) -> tuple[int, int, int, int, int, int]:
        return (
            metadata.st_mode,
            metadata.st_dev,
            metadata.st_ino,
            metadata.st_size,
            metadata.st_mtime_ns,
            metadata.st_ctime_ns,
        )

    for relative in relative_paths:
        encoded = relative.encode("utf-8", errors="surrogateescape")
        relative_path = Path(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts:
            raise ValueError(f"git returned an unsafe effective worktree path: {relative!r}")
        path = root / relative
        try:
            before = path.lstat()
        except OSError as error:
            raise ValueError(
                f"effective worktree entry disappeared or is unreadable: {relative!r}"
            ) from error
        if stat.S_ISLNK(before.st_mode):
            try:
                target_text = os.readlink(path)
                after = path.lstat()
                target_after = os.readlink(path)
            except OSError as error:
                raise ValueError(
                    f"effective worktree symlink changed while hashing: {relative!r}"
                ) from error
            if metadata_signature(before) != metadata_signature(after) or target_text != target_after:
                raise ValueError(
                    f"effective worktree symlink changed while hashing: {relative!r}"
                )
            target = target_text.encode("utf-8", errors="surrogateescape")
            frame_prefix(EFFECTIVE_WORKTREE_SYMLINK_ENTRY_TAG, encoded, len(target))
            digest.update(target)
        elif stat.S_ISREG(before.st_mode):
            with path.open("rb") as stream:
                opened = os.fstat(stream.fileno())
                if (
                    not stat.S_ISREG(opened.st_mode)
                    or metadata_signature(opened) != metadata_signature(before)
                ):
                    raise ValueError(
                        f"effective worktree file changed before hashing: {relative!r}"
                    )
                frame_prefix(
                    EFFECTIVE_WORKTREE_REGULAR_ENTRY_TAG,
                    encoded,
                    opened.st_size,
                )
                bytes_read = 0
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    bytes_read += len(chunk)
                    digest.update(chunk)
                opened_after = os.fstat(stream.fileno())
            try:
                path_after = path.lstat()
            except OSError as error:
                raise ValueError(
                    f"effective worktree file changed while hashing: {relative!r}"
                ) from error
            if (
                bytes_read != opened.st_size
                or metadata_signature(opened_after) != metadata_signature(opened)
                or metadata_signature(path_after) != metadata_signature(opened)
            ):
                raise ValueError(
                    f"effective worktree file changed while hashing: {relative!r}"
                )
        else:
            raise ValueError(
                f"effective worktree contains a missing or special entry: {relative!r}"
            )
    return digest.hexdigest(), len(relative_paths)


def effective_worktree_digest_self_test() -> None:
    def legacy_effective_worktree_digest(root: Path) -> str:
        raw = run_git(root, "ls-files", "-co", "--exclude-standard", "-z", binary=True)
        assert isinstance(raw, bytes)
        relative_paths = sorted(
            (
                item.decode("utf-8", errors="surrogateescape")
                for item in raw.split(b"\0")
                if item
            ),
            key=lambda item: item.encode("utf-8", errors="surrogateescape"),
        )
        digest = hashlib.sha256()
        for relative in relative_paths:
            encoded = relative.encode("utf-8", errors="surrogateescape")
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)
            path = root / relative
            if path.is_symlink():
                target = os.readlink(path).encode("utf-8", errors="surrogateescape")
                digest.update(b"L")
                digest.update(len(target).to_bytes(8, "big"))
                digest.update(target)
            elif path.is_file():
                digest.update(b"F")
                digest.update(path.read_bytes())
            else:
                digest.update(b"M")
        return digest.hexdigest()

    with tempfile.TemporaryDirectory(
        prefix="ytt-perf-effective-worktree-digest-self-test-"
    ) as temporary:
        base = Path(temporary)
        one_file = base / "one-file"
        two_files = base / "two-files"
        missing_file = base / "missing-file"
        for repository in (one_file, two_files, missing_file):
            repository.mkdir()
            run_git(repository, "init", "--quiet")

        prefix = b"first-content"
        suffix = b"second-content"
        encoded_second_entry = (
            len(b"b").to_bytes(8, "big") + b"b" + b"F" + suffix
        )
        (one_file / "a").write_bytes(prefix + encoded_second_entry)
        (two_files / "a").write_bytes(prefix)
        (two_files / "b").write_bytes(suffix)
        if legacy_effective_worktree_digest(
            one_file
        ) != legacy_effective_worktree_digest(two_files):
            raise AssertionError(
                "effective worktree collision fixture does not reproduce legacy ambiguity"
            )
        one_digest, one_count = effective_worktree_digest(one_file)
        two_digest, two_count = effective_worktree_digest(two_files)
        if (one_count, two_count) != (1, 2):
            raise AssertionError("effective worktree collision fixture has unexpected entries")
        if one_digest == two_digest:
            raise AssertionError(
                "length-framed effective worktree digest retained a legacy collision"
            )

        link = two_files / "link"
        try:
            link.symlink_to("a")
        except OSError:
            pass
        else:
            first_link_digest, _ = effective_worktree_digest(two_files)
            link.unlink()
            link.symlink_to("b")
            second_link_digest, _ = effective_worktree_digest(two_files)
            if first_link_digest == second_link_digest:
                raise AssertionError(
                    "effective worktree digest did not bind the symlink target"
                )

        tracked = missing_file / "tracked"
        tracked.write_bytes(b"tracked-content")
        run_git(missing_file, "add", "--", "tracked")
        tracked.unlink()
        try:
            effective_worktree_digest(missing_file)
        except ValueError:
            pass
        else:
            raise AssertionError(
                "effective worktree digest accepted a missing tracked entry"
            )


def effective_cargo_home() -> Path:
    raw = os.environ.get("CARGO_HOME")
    if raw:
        return Path(raw).expanduser().resolve()
    return (Path.home() / ".cargo").resolve()


def cargo_config_chain(root: Path) -> list[dict[str, Any]]:
    """Bind every Cargo config source that can affect a build from ``root``.

    Cargo walks from the invocation directory through every ancestor and also reads the
    effective CARGO_HOME.  Ignored files are intentionally included here: git cleanliness
    alone is not a build-input contract.
    """
    root = root.resolve()
    entries: list[dict[str, Any]] = []
    current = root
    depth = 0
    while True:
        for name in ("config.toml", "config"):
            path = current / ".cargo" / name
            if path.is_file():
                entries.append(
                    {
                        "scope": "source" if depth == 0 else "ancestor",
                        "ancestor_depth": depth,
                        "name": name,
                        **identity_for_file(path),
                    }
                )
        if current.parent == current:
            break
        current = current.parent
        depth += 1

    cargo_home = effective_cargo_home()
    for name in ("config.toml", "config"):
        path = cargo_home / name
        if path.is_file():
            entries.append(
                {
                    "scope": "cargo_home",
                    "ancestor_depth": None,
                    "name": name,
                    **identity_for_file(path),
                }
            )
    return entries


def comparable_cargo_config_chain(chain: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "scope": entry["scope"],
            "ancestor_depth": entry["ancestor_depth"],
            "name": entry["name"],
            "bytes": entry["bytes"],
            "sha256": entry["sha256"],
        }
        for entry in chain
    ]


def source_identity(root: Path, build_command: str | None = None) -> dict[str, Any]:
    root = root.resolve()
    if not root.is_dir():
        raise ValueError(f"source root does not exist: {root}")
    top_level = Path(str(run_git(root, "rev-parse", "--show-toplevel"))).resolve()
    if top_level != root:
        raise ValueError(f"source root must be the git top level: {root} (actual {top_level})")
    lockfile = root / "Cargo.lock"
    if not lockfile.is_file():
        raise ValueError(f"source root has no Cargo.lock: {root}")
    status = run_git(root, "status", "--porcelain=v1", "--untracked-files=all", "-z", binary=True)
    assert isinstance(status, bytes)
    effective_sha256, entry_count = effective_worktree_digest(root)
    identity = {
        "root": str(root),
        "head": str(run_git(root, "rev-parse", "HEAD^{commit}")),
        "tree": str(run_git(root, "rev-parse", "HEAD^{tree}")),
        "dirty": bool(status),
        "status_sha256": hashlib.sha256(status).hexdigest(),
        "effective_worktree_sha256": effective_sha256,
        "effective_worktree_entries": entry_count,
        "cargo_lock": {
            "path": "Cargo.lock",
            "bytes": lockfile.stat().st_size,
            "sha256": sha256_file(lockfile),
        },
        "cargo_config_chain": cargo_config_chain(root),
        "package_version": package_version_from_cargo_toml(root / "Cargo.toml"),
    }
    if build_command is not None:
        identity["build_command"] = build_command
    return identity


def untracked_paths(root: Path) -> list[str]:
    raw = run_git(root, "ls-files", "--others", "--exclude-standard", "-z", binary=True)
    assert isinstance(raw, bytes)
    return sorted(
        item.decode("utf-8", errors="surrogateescape")
        for item in raw.split(b"\0")
        if item
    )


def tracked_worktree_is_clean(root: Path) -> bool:
    for arguments in (("diff", "--quiet"), ("diff", "--cached", "--quiet")):
        result = subprocess.run(
            ["git", "-C", str(root), *arguments],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode == 1:
            return False
        if result.returncode != 0:
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            raise ValueError(f"git {' '.join(arguments)} failed in {root}: {detail}")
    return True


def refresh_origin_main(candidate_root: Path) -> str:
    result = subprocess.run(
        [
            "git",
            "-C",
            str(candidate_root),
            "fetch",
            "--no-tags",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"cannot refresh candidate origin/main: {detail}")
    return str(run_git(candidate_root, "rev-parse", "origin/main^{commit}"))


def validate_source_contract(
    baseline_root: Path,
    candidate_root: Path,
    render: bool,
    *,
    refresh: bool,
) -> tuple[dict[str, Any], dict[str, Any], str, str]:
    baseline_root = baseline_root.resolve()
    candidate_root = candidate_root.resolve()
    expected_baseline = (
        refresh_origin_main(candidate_root)
        if refresh
        else str(run_git(candidate_root, "rev-parse", "origin/main^{commit}"))
    )
    expected_candidate = str(run_git(candidate_root, "rev-parse", "HEAD^{commit}"))
    ancestor = subprocess.run(
        [
            "git",
            "-C",
            str(candidate_root),
            "merge-base",
            "--is-ancestor",
            expected_baseline,
            expected_candidate,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )
    if ancestor.returncode == 1:
        raise ValueError("candidate HEAD must descend from the exact origin/main baseline")
    if ancestor.returncode != 0:
        detail = ancestor.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"cannot verify candidate ancestry: {detail}")
    if not tracked_worktree_is_clean(candidate_root) or untracked_paths(candidate_root):
        raise ValueError("candidate source must be an exact clean HEAD with no untracked files")
    if str(run_git(baseline_root, "rev-parse", "HEAD^{commit}")) != expected_baseline:
        raise ValueError(
            "baseline HEAD must equal the candidate repository's exact origin/main OID "
            f"{expected_baseline}"
        )
    if not tracked_worktree_is_clean(baseline_root):
        raise ValueError("baseline source has tracked or staged changes")

    render_relative = "examples/tui_render_perf.rs"
    candidate_harness = candidate_root / render_relative
    baseline_harness = baseline_root / render_relative
    baseline_untracked = untracked_paths(baseline_root)
    if render:
        if not candidate_harness.is_file():
            raise ValueError(f"candidate render harness is missing: {candidate_harness}")
        if not baseline_harness.exists():
            baseline_harness.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(candidate_harness, baseline_harness)
        baseline_untracked = untracked_paths(baseline_root)
    if baseline_untracked not in ([], [render_relative]):
        raise ValueError(
            "baseline may contain only the untracked render harness; found "
            f"{baseline_untracked}"
        )
    if baseline_untracked == [render_relative]:
        if not candidate_harness.is_file():
            raise ValueError("candidate has no render harness to authenticate the baseline copy")
        if baseline_harness.read_bytes() != candidate_harness.read_bytes():
            raise ValueError("baseline untracked render harness is not byte-identical to candidate")
    if render and baseline_harness.read_bytes() != candidate_harness.read_bytes():
        raise ValueError("baseline render harness is not byte-identical to candidate")

    baseline_identity = source_identity(baseline_root)
    candidate_identity = source_identity(candidate_root)
    if baseline_identity["head"] != expected_baseline:
        raise ValueError("baseline source identity changed while it was being captured")
    if candidate_identity["head"] != expected_candidate:
        raise ValueError("candidate source identity changed while it was being captured")
    baseline_configs = comparable_cargo_config_chain(
        baseline_identity["cargo_config_chain"]
    )
    candidate_configs = comparable_cargo_config_chain(
        candidate_identity["cargo_config_chain"]
    )
    if baseline_configs != candidate_configs:
        raise ValueError(
            "baseline/candidate effective Cargo config chains differ; ignored and ancestor "
            f"configs are build inputs (baseline={baseline_configs}, candidate={candidate_configs})"
        )
    return baseline_identity, candidate_identity, expected_baseline, expected_candidate


def require_ignored_evidence_root(source_root: Path, evidence_root: Path) -> None:
    source_root = source_root.resolve()
    evidence_root = evidence_root.resolve()
    try:
        relative = evidence_root.relative_to(source_root).as_posix()
    except ValueError:
        return
    result = subprocess.run(
        ["git", "-C", str(source_root), "check-ignore", "-q", "--", relative],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode == 0:
        return
    if result.returncode != 1:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"git check-ignore failed for evidence root {evidence_root}: {detail}")
    raise ValueError(
        f"evidence root inside source tree must be gitignored so measurements cannot alter "
        f"the source digest: {evidence_root}"
    )


def seed_cache_policy(root: Path) -> dict[str, Any]:
    path = root / "stores" / "config" / "config.json"
    if not path.is_file():
        return {"config_present": False}
    try:
        document = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicate_json_keys
        )
    except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(f"cannot inspect seed cache policy at {path}: {error}") from error
    mpv = document.get("audio", {}).get("mpv", {}) if isinstance(document, dict) else {}
    if not isinstance(mpv, dict):
        mpv = {}
    fields = ("cache_forward", "cache_back", "_cache_defaults_revision")
    return {
        "config_present": True,
        **{
            field: {"present": field in mpv, "value": mpv.get(field)}
            for field in fields
        },
    }


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def atomic_text(path: Path, value: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(value, encoding="utf-8")
    os.replace(temporary, path)


def atomic_bytes(path: Path, value: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_bytes(value)
    os.replace(temporary, path)


def resolve_build_tool(name: str, cwd: Path, environment: dict[str, str]) -> Path:
    path_value = environment.get("PATH")
    if not path_value:
        raise ValueError("controlled build environment has no non-empty PATH")
    suffixes = [""]
    if os.name == "nt" and not Path(name).suffix:
        suffixes = [
            suffix.lower()
            for suffix in environment.get("PATHEXT", ".COM;.EXE;.BAT;.CMD").split(os.pathsep)
            if suffix
        ]
    for raw_directory in path_value.split(os.pathsep):
        directory = Path(raw_directory or ".")
        if not directory.is_absolute():
            directory = cwd / directory
        for suffix in suffixes:
            candidate = Path(os.path.abspath(directory / f"{name}{suffix}"))
            if candidate.is_file() and (os.name == "nt" or os.access(candidate, os.X_OK)):
                return candidate
    raise ValueError(f"required build tool is not on controlled PATH from {cwd}: {name}")


def exact_tool_output(
    executable: Path,
    arguments: list[str],
    *,
    cwd: Path,
    environment: dict[str, str],
    label: str,
) -> str:
    completed = subprocess.run(
        [str(executable), *arguments],
        cwd=cwd,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        raise ValueError(f"{label} failed in {cwd}: {detail}")
    output = (completed.stdout or completed.stderr).strip()
    if not output:
        raise ValueError(f"{label} produced no identity output in {cwd}")
    return output


def controlled_build_environment() -> dict[str, str]:
    environment = {
        name: value
        for name in CONTROLLED_BUILD_ENV_ALLOWLIST
        if (value := os.environ.get(name)) is not None
    }
    environment["CARGO_HOME"] = str(effective_cargo_home())
    return environment


def captured_build_environment(
    environment: dict[str, str] | None = None,
) -> dict[str, Any]:
    environment = dict(
        controlled_build_environment() if environment is None else environment
    )
    return {
        "policy": "allowlist-v1",
        "allowlist": list(CONTROLLED_BUILD_ENV_ALLOWLIST),
        "variables": environment,
        "explicitly_removed_prefixes": [
            "CARGO_BUILD_",
            "CARGO_TARGET_",
            "CARGO_PROFILE_",
        ],
        "explicitly_removed": [
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
        ],
    }


def toolchain_selector_chain(root: Path) -> list[dict[str, Any]]:
    root = root.resolve()
    selectors: list[dict[str, Any]] = []
    current = root
    depth = 0
    while True:
        for name in ("rust-toolchain.toml", "rust-toolchain"):
            path = current / name
            if path.is_file():
                selectors.append(
                    {
                        "scope": "source" if depth == 0 else "ancestor",
                        "ancestor_depth": depth,
                        "name": name,
                        "selector_path": str(path),
                        **identity_for_file(path),
                    }
                )
        if current.parent == current:
            break
        current = current.parent
        depth += 1
    return selectors


def relevant_rustup_overrides(root: Path, output: str) -> list[dict[str, Any]]:
    root = root.resolve()
    ancestors: list[tuple[int, Path]] = []
    current = root
    depth = 0
    while True:
        ancestors.append((depth, current))
        if current.parent == current:
            break
        current = current.parent
        depth += 1
    overrides: list[dict[str, Any]] = []
    for line in output.splitlines():
        if not line.strip() or line.strip().lower() == "no overrides":
            continue
        for ancestor_depth, ancestor in ancestors:
            prefix = str(ancestor)
            if line.startswith(prefix) and line[len(prefix) : len(prefix) + 1].isspace():
                toolchain = line[len(prefix) :].strip()
                if toolchain:
                    overrides.append(
                        {
                            "directory": prefix,
                            "scope": "source" if ancestor_depth == 0 else "ancestor",
                            "ancestor_depth": ancestor_depth,
                            "toolchain": toolchain,
                        }
                    )
                break
    overrides.sort(key=lambda item: int(item["ancestor_depth"]))
    return overrides


def capture_effective_toolchain(
    source_root: Path,
    environment: dict[str, str] | None = None,
) -> dict[str, Any]:
    source_root = source_root.resolve()
    environment = dict(
        controlled_build_environment() if environment is None else environment
    )
    rustup_path: Path | None
    try:
        rustup_path = resolve_build_tool("rustup", source_root, environment)
    except ValueError:
        rustup_path = None
    rustup: dict[str, Any]
    if rustup_path is None:
        rustup = {
            "available": False,
            "rustup_toolchain_environment": environment.get("RUSTUP_TOOLCHAIN"),
        }
    else:
        rustup_executable = identity_for_file(rustup_path)
        active = exact_tool_output(
            rustup_path,
            ["show", "active-toolchain"],
            cwd=source_root,
            environment=environment,
            label="rustup show active-toolchain",
        )
        override_list = exact_tool_output(
            rustup_path,
            ["override", "list"],
            cwd=source_root,
            environment=environment,
            label="rustup override list",
        )
        rustup = {
            "available": True,
            "invocation_path": str(rustup_path),
            "executable": rustup_executable,
            "version": exact_tool_output(
                rustup_path,
                ["--version"],
                cwd=source_root,
                environment=environment,
                label="rustup --version",
            ),
            "active_toolchain": active,
            "active_toolchain_name": active.split(maxsplit=1)[0],
            "relevant_directory_overrides": relevant_rustup_overrides(
                source_root, override_list
            ),
            "rustup_toolchain_environment": environment.get("RUSTUP_TOOLCHAIN"),
        }

    tools: dict[str, dict[str, Any]] = {}
    rustup_executable = rustup.get("executable")
    for name, version_arguments in (("cargo", ["-Vv"]), ("rustc", ["-vV"])):
        invocation_path = resolve_build_tool(name, source_root, environment)
        invocation_executable = identity_for_file(invocation_path)
        selected_executable = invocation_executable
        selected_via = "controlled_path"
        if (
            rustup_path is not None
            and isinstance(rustup_executable, dict)
            and invocation_executable["bytes"] == rustup_executable["bytes"]
            and invocation_executable["sha256"] == rustup_executable["sha256"]
        ):
            selected_path = Path(
                exact_tool_output(
                    rustup_path,
                    ["which", name],
                    cwd=source_root,
                    environment=environment,
                    label=f"rustup which {name}",
                )
            )
            if not selected_path.is_absolute():
                selected_path = source_root / selected_path
            selected_executable = identity_for_file(selected_path)
            selected_via = "rustup_proxy"
        tools[name] = {
            "invocation_path": str(invocation_path),
            "invocation_executable": invocation_executable,
            "selected_via": selected_via,
            "selected_executable": selected_executable,
            "version": exact_tool_output(
                invocation_path,
                version_arguments,
                cwd=source_root,
                environment=environment,
                label=f"{name} {' '.join(version_arguments)}",
            ),
        }
    return {
        "source_root": str(source_root),
        "selector_chain": toolchain_selector_chain(source_root),
        "rustup": rustup,
        **tools,
    }


def comparable_effective_toolchain(identity: dict[str, Any]) -> dict[str, Any]:
    comparable: dict[str, Any] = {}
    for name in ("cargo", "rustc"):
        tool = identity[name]
        comparable[name] = {
            field: tool[field]
            for field in (
                "invocation_path",
                "invocation_executable",
                "selected_via",
                "selected_executable",
                "version",
            )
        }
    if any(identity[name]["selected_via"] == "rustup_proxy" for name in ("cargo", "rustc")):
        comparable["rustup_active_toolchain_name"] = identity["rustup"].get(
            "active_toolchain_name"
        )
    return comparable


def require_matching_effective_toolchains(toolchains: dict[str, dict[str, Any]]) -> None:
    if set(toolchains) != {"baseline", "candidate"}:
        raise ValueError("toolchain identities must contain exactly baseline and candidate")
    baseline = comparable_effective_toolchain(toolchains["baseline"])
    candidate = comparable_effective_toolchain(toolchains["candidate"])
    if baseline != candidate:
        raise ValueError(
            "baseline/candidate effective Cargo and Rust toolchains differ before build "
            f"(baseline={baseline}, candidate={candidate})"
        )


def capture_build_toolchains(
    baseline_root: Path,
    candidate_root: Path,
    environment: dict[str, str] | None = None,
) -> dict[str, dict[str, Any]]:
    environment = dict(
        controlled_build_environment() if environment is None else environment
    )
    toolchains = {
        "baseline": capture_effective_toolchain(baseline_root, environment),
        "candidate": capture_effective_toolchain(candidate_root, environment),
    }
    require_matching_effective_toolchains(toolchains)
    return toolchains


def validate_recorded_build_toolchains(
    recorded: Any,
    baseline_root: Path,
    candidate_root: Path,
    environment: dict[str, str] | None = None,
) -> dict[str, dict[str, Any]]:
    current = capture_build_toolchains(
        baseline_root, candidate_root, environment
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "source-specific toolchains",
        recorded,
        current,
    )
    return current


def summarized_toolchain(identity: dict[str, Any]) -> dict[str, str]:
    return {name: str(identity[name]["version"]) for name in ("cargo", "rustc")}


def pinned_compiler_binding(identity: dict[str, Any]) -> dict[str, Any]:
    rustc = identity["rustc"]["selected_executable"]
    rustc_path = Path(str(rustc["path"]))
    if not rustc_path.is_absolute():
        raise ValueError("selected rustc path must be absolute")
    require_artifact_value(
        rustc_path,
        "selected rustc executable",
        identity_for_file(rustc_path),
        rustc,
    )
    return {
        "policy": "absolute_rustc_without_wrappers_v1",
        "rustc": rustc,
        "environment": {
            "RUSTC": str(rustc_path),
            # Empty explicitly overrides build.rustc-wrapper from any captured Cargo config.
            "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
        },
    }


def build_specs(render: bool) -> list[tuple[str, list[str], dict[str, str]]]:
    if render:
        return [
            ("baseline", ["--example", "tui_render_perf"], {"tui_render_perf": "baseline_render"}),
            ("candidate", ["--example", "tui_render_perf"], {"tui_render_perf": "candidate_render"}),
        ]
    return [
        ("baseline", ["--bin", "ytt"], {"ytt": "baseline_ytt"}),
        (
            "candidate",
            [
                "--bin",
                "ytt",
                "--example",
                "tui_perf_sampler",
                "--example",
                "tui_perf_control",
            ],
            {
                "ytt": "candidate_ytt",
                "tui_perf_sampler": "sampler",
                "tui_perf_control": "controller",
            },
        ),
    ]


def run_fixed_cargo_build(
    source_root: Path,
    target_dir: Path,
    selectors: list[str],
    environment: dict[str, str],
    expected_toolchain: dict[str, Any],
) -> tuple[list[str], dict[str, Path]]:
    command = [
        "cargo",
        "build",
        "--release",
        "--locked",
        "--message-format=json-render-diagnostics",
        *selectors,
    ]
    require_artifact_value(
        source_root,
        "effective toolchain immediately before build",
        capture_effective_toolchain(source_root, environment),
        expected_toolchain,
    )
    cargo_path = Path(expected_toolchain["cargo"]["invocation_path"])
    require_artifact_value(
        source_root,
        "Cargo invocation executable before build",
        identity_for_file(cargo_path),
        expected_toolchain["cargo"]["invocation_executable"],
    )
    compiler_binding = pinned_compiler_binding(expected_toolchain)
    build_environment = dict(environment)
    build_environment["CARGO_TARGET_DIR"] = str(target_dir.resolve())
    build_environment.update(compiler_binding["environment"])
    completed = subprocess.run(
        [str(cargo_path), *command[1:]],
        cwd=source_root,
        env=build_environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise ValueError(
            f"fixed cargo build failed in {source_root}:\n{completed.stderr.strip()}"
        )
    require_artifact_value(
        source_root,
        "effective toolchain immediately after build",
        capture_effective_toolchain(source_root, environment),
        expected_toolchain,
    )
    require_artifact_value(
        source_root,
        "pinned compiler binding immediately after build",
        pinned_compiler_binding(expected_toolchain),
        compiler_binding,
    )
    executables: dict[str, Path] = {}
    for line in completed.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        target = message.get("target")
        if isinstance(target, dict) and isinstance(target.get("name"), str):
            executables[target["name"]] = Path(message["executable"]).resolve()
    return command, executables


def harness_source_identities(
    baseline_root: Path, candidate_root: Path, render: bool
) -> dict[str, Any]:
    names = ["tui_render_perf.rs"] if render else ["tui_perf_sampler.rs", "tui_perf_control.rs"]
    identities: dict[str, Any] = {}
    for name in names:
        candidate = candidate_root / "examples" / name
        identities[name] = {"candidate": identity_for_file(candidate)}
        if render:
            baseline = baseline_root / "examples" / name
            baseline_identity = identity_for_file(baseline)
            if baseline_identity["sha256"] != identities[name]["candidate"]["sha256"]:
                raise ValueError("baseline and candidate render harness sources differ")
            identities[name]["baseline"] = baseline_identity
    return identities


def copy_receipted_binary(source: Path, destination_root: Path, label: str) -> dict[str, Any]:
    suffix = source.suffix if source.suffix.lower() == ".exe" else ""
    destination = destination_root / "binaries" / f"{label}{suffix}"
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(destination.name + ".tmp")
    shutil.copy2(source, temporary)
    os.replace(temporary, destination)
    copied = identity_for_file(destination)
    built = identity_for_file(source)
    if copied["sha256"] != built["sha256"] or copied["bytes"] != built["bytes"]:
        raise ValueError(f"copied build output changed for {label}")
    return {**copied, "built_output": built}


def validate_build_receipt(
    receipt: dict[str, Any],
    baseline_root: Path,
    candidate_root: Path,
    render: bool,
    *,
    refresh: bool,
) -> None:
    if receipt.get("schema") != BUILD_RECEIPT_SCHEMA:
        raise ValueError("build receipt has an unsupported schema")
    if receipt.get("build_kind") != ("render" if render else "process"):
        raise ValueError("build receipt kind does not match the selected scenario")
    baseline, candidate, expected_baseline, expected_candidate = validate_source_contract(
        baseline_root, candidate_root, render, refresh=refresh
    )
    require_artifact_value(Path("<build-receipt>"), "baseline OID", receipt.get("baseline_oid"), expected_baseline)
    require_artifact_value(Path("<build-receipt>"), "candidate OID", receipt.get("candidate_oid"), expected_candidate)
    require_artifact_value(Path("<build-receipt>"), "baseline source", receipt.get("sources", {}).get("baseline"), baseline)
    require_artifact_value(Path("<build-receipt>"), "candidate source", receipt.get("sources", {}).get("candidate"), candidate)
    environment = controlled_build_environment()
    toolchains = validate_recorded_build_toolchains(
        receipt.get("toolchains"),
        baseline_root,
        candidate_root,
        environment,
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "common toolchain summary",
        receipt.get("toolchain"),
        summarized_toolchain(toolchains["baseline"]),
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "build environment",
        receipt.get("build_environment"),
        captured_build_environment(environment),
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "compiler bindings",
        receipt.get("compiler_bindings"),
        {
            role: pinned_compiler_binding(toolchains[role])
            for role in ("baseline", "candidate")
        },
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "harness sources",
        receipt.get("harness_sources"),
        harness_source_identities(baseline_root, candidate_root, render),
    )
    require_artifact_value(
        Path("<build-receipt>"),
        "orchestrator",
        receipt.get("orchestrator"),
        identity_for_file(Path(__file__)),
    )
    artifacts = receipt.get("artifacts")
    if not isinstance(artifacts, dict):
        raise ValueError("build receipt artifacts must be an object")
    expected_labels = {
        label
        for _role, _selectors, mapping in build_specs(render)
        for label in mapping.values()
    }
    if set(artifacts) != expected_labels:
        raise ValueError(
            f"build receipt artifacts are {sorted(artifacts)}, expected {sorted(expected_labels)}"
        )
    expected_commands: dict[str, tuple[str, list[str]]] = {}
    for role, selectors, mapping in build_specs(render):
        command = [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--message-format=json-render-diagnostics",
            *selectors,
        ]
        for label in mapping.values():
            expected_commands[label] = (role, command)
    for label, artifact in artifacts.items():
        if not isinstance(artifact, dict):
            raise ValueError(f"build receipt artifact {label} must be an object")
        role, command = expected_commands[label]
        require_artifact_value(Path("<build-receipt>"), f"{label} source role", artifact.get("source_role"), role)
        require_artifact_value(Path("<build-receipt>"), f"{label} build command", artifact.get("build_command"), command)
        path = Path(str(artifact.get("path", "")))
        current = identity_for_file(path)
        for field in ("path", "bytes", "sha256"):
            require_artifact_value(Path("<build-receipt>"), f"{label} {field}", artifact.get(field), current[field])


def command_build(args: argparse.Namespace) -> int:
    if args.output.exists():
        raise ValueError("build receipt output must name a new path")
    document, _ = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.scenario)
    render = scenario["id"] == "render_and_interaction"
    baseline_root = args.baseline_root.resolve()
    candidate_root = args.candidate_root.resolve()
    evidence_root = args.output.resolve().parent
    require_ignored_evidence_root(baseline_root, evidence_root)
    require_ignored_evidence_root(candidate_root, evidence_root)

    target_root = args.target_root.resolve()
    if target_root.exists():
        raise ValueError("--target-root must be a new path for a fresh controlled build")
    baseline, candidate, baseline_oid, candidate_oid = validate_source_contract(
        baseline_root, candidate_root, render, refresh=True
    )
    harness_sources = harness_source_identities(baseline_root, candidate_root, render)
    environment = controlled_build_environment()
    toolchains = capture_build_toolchains(
        baseline_root, candidate_root, environment
    )
    toolchain = summarized_toolchain(toolchains["baseline"])
    build_environment = captured_build_environment(environment)
    compiler_bindings = {
        role: pinned_compiler_binding(toolchains[role])
        for role in ("baseline", "candidate")
    }
    target_root.mkdir(parents=True)
    sources = {"baseline": baseline, "candidate": candidate}
    artifacts: dict[str, Any] = {}
    for role, selectors, mapping in build_specs(render):
        source_root = baseline_root if role == "baseline" else candidate_root
        target_dir = target_root / role
        command, executables = run_fixed_cargo_build(
            source_root,
            target_dir,
            selectors,
            environment,
            toolchains[role],
        )
        missing = sorted(set(mapping) - set(executables))
        if missing:
            raise ValueError(f"cargo did not report expected executables: {missing}")
        for cargo_name, label in mapping.items():
            copied = copy_receipted_binary(executables[cargo_name], evidence_root, label)
            artifacts[label] = {
                **copied,
                "source_role": role,
                "build_command": command,
                "target_dir": str(target_dir),
            }

    baseline_after, candidate_after, baseline_after_oid, candidate_after_oid = validate_source_contract(
        baseline_root, candidate_root, render, refresh=False
    )
    if (baseline_after, candidate_after, baseline_after_oid, candidate_after_oid) != (
        baseline,
        candidate,
        baseline_oid,
        candidate_oid,
    ):
        raise ValueError("source identity changed during controlled build")
    receipt = {
        "schema": BUILD_RECEIPT_SCHEMA,
        "build_kind": "render" if render else "process",
        "generated_unix_s": int(time.time()),
        "baseline_oid": baseline_oid,
        "candidate_oid": candidate_oid,
        "sources": sources,
        "toolchain": toolchain,
        "toolchains": toolchains,
        "build_environment": build_environment,
        "compiler_bindings": compiler_bindings,
        "harness_sources": harness_sources,
        "orchestrator": identity_for_file(Path(__file__)),
        "artifacts": artifacts,
    }
    atomic_json(args.output, receipt)
    validate_build_receipt(receipt, baseline_root, candidate_root, render, refresh=False)
    print(json.dumps({"ok": True, "output": str(args.output.resolve()), "fresh_build": True}))
    return 0


def command_receipt(args: argparse.Namespace) -> int:
    receipt = load_json_object(args.receipt)
    if receipt.get("schema") != BUILD_RECEIPT_SCHEMA:
        raise ValueError("unsupported build receipt schema")
    artifact = receipt.get("artifacts", {}).get(args.artifact)
    if not isinstance(artifact, dict):
        raise ValueError(f"build receipt has no artifact {args.artifact!r}")
    value = dotted(artifact, args.field)
    print(value if not isinstance(value, (dict, list)) else json.dumps(value, separators=(",", ":")))
    return 0


def checksum_targets(root: Path, output: Path) -> list[Path]:
    root = root.resolve()
    output = output.resolve()
    temporary_output = output.with_name(output.name + ".tmp").resolve()
    return sorted(
        (
            path
            for path in root.rglob("*")
            if path.is_file()
            and path.resolve() != output
            and path.resolve() != temporary_output
        ),
        key=lambda path: path.relative_to(root).as_posix().encode("utf-8"),
    )


def write_checksums(root: Path, output: Path) -> int:
    root = root.resolve()
    output = output.resolve()
    try:
        output.relative_to(root)
    except ValueError as error:
        raise ValueError("checksum output must stay inside its root") from error
    lines = []
    for path in checksum_targets(root, output):
        relative = path.relative_to(root).as_posix()
        if "\n" in relative or "\r" in relative:
            raise ValueError(f"checksum path contains a newline: {relative!r}")
        lines.append(f"{sha256_file(path)}  {relative}")
    atomic_text(output, "\n".join(lines) + "\n")
    return len(lines)


def verify_checksums(root: Path, output: Path) -> int:
    root = root.resolve()
    output = output.resolve()
    if not output.is_file():
        raise ValueError(f"checksum file does not exist: {output}")
    expected_paths = {
        path.relative_to(root).as_posix(): path for path in checksum_targets(root, output)
    }
    listed: dict[str, str] = {}
    for number, line in enumerate(output.read_text(encoding="utf-8").splitlines(), start=1):
        digest, separator, relative = line.partition("  ")
        if (
            not separator
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
            or not relative
        ):
            raise ValueError(f"{output}:{number}: malformed SHA256SUMS line")
        if relative in listed:
            raise ValueError(f"{output}:{number}: duplicate checksum path {relative!r}")
        listed[relative] = digest
    if set(listed) != set(expected_paths):
        missing = sorted(set(expected_paths) - set(listed))
        extra = sorted(set(listed) - set(expected_paths))
        raise ValueError(f"checksum inventory mismatch; missing={missing}, extra={extra}")
    for relative, expected in listed.items():
        actual = sha256_file(expected_paths[relative])
        if actual != expected:
            raise ValueError(
                f"checksum mismatch for {relative}: expected {expected}, actual {actual}"
            )
    return len(listed)


def command_create_checksums(args: argparse.Namespace) -> int:
    count = write_checksums(args.root, args.output)
    verified = verify_checksums(args.root, args.output)
    if verified != count:
        raise ValueError("checksum write/verify count mismatch")
    print(
        json.dumps(
            {
                "ok": True,
                "mode": "create_overwrite",
                "output": str(args.output.resolve()),
                "files": count,
            }
        )
    )
    return 0


def command_verify_checksums(args: argparse.Namespace) -> int:
    before = sha256_file(args.output)
    count = verify_checksums(args.root, args.output)
    after = sha256_file(args.output)
    if after != before:
        raise ValueError("read-only checksum verification changed SHA256SUMS")
    print(
        json.dumps(
            {
                "ok": True,
                "mode": "verify_read_only",
                "output": str(args.output.resolve()),
                "files": count,
            }
        )
    )
    return 0


def load_scenarios(path: Path) -> tuple[dict[str, Any], str]:
    try:
        raw = path.read_bytes()
        document = json.loads(raw, object_pairs_hook=reject_duplicate_json_keys)
    except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(f"cannot read scenario file {path}: {error}") from error
    validate_scenarios(document)
    return document, hashlib.sha256(raw).hexdigest()


def scenario_finite_number(value: Any) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(float(value))
    except (OverflowError, ValueError):
        return False


def validate_scenarios(document: dict[str, Any]) -> None:
    if document.get("schema") != "ytt.tui-perf.scenarios.v1":
        raise ValueError("scenario schema must be ytt.tui-perf.scenarios.v1")
    if document.get("version") != 1:
        raise ValueError("scenario version must be 1")
    stats = document.get("statistics")
    if not isinstance(stats, dict):
        raise ValueError("statistics must be an object")
    bootstrap_resamples = stats.get("bootstrap_resamples")
    if (
        not isinstance(bootstrap_resamples, int)
        or isinstance(bootstrap_resamples, bool)
        or not 1 <= bootstrap_resamples <= MAX_BOOTSTRAP_RESAMPLES
    ):
        raise ValueError(
            "statistics.bootstrap_resamples must be an integer in "
            f"[1,{MAX_BOOTSTRAP_RESAMPLES}]"
        )
    seed = stats.get("seed")
    if (
        not isinstance(seed, int)
        or isinstance(seed, bool)
        or not 0 <= seed <= MAX_STATISTICS_SEED
    ):
        raise ValueError(
            "statistics.seed must be an integer in "
            f"[0,{MAX_STATISTICS_SEED}]"
        )
    confidence = stats.get("one_sided_confidence")
    if (
        not scenario_finite_number(confidence)
        or not 0 < float(confidence) < 1
    ):
        raise ValueError("statistics.one_sided_confidence must be finite and in (0,1)")
    baseline_cv_max = stats.get("baseline_cv_max")
    if (
        not scenario_finite_number(baseline_cv_max)
        or float(baseline_cv_max) < 0
    ):
        raise ValueError("statistics.baseline_cv_max must be finite and non-negative")
    sampling = document.get("sampling")
    if not isinstance(sampling, dict):
        raise ValueError("sampling must be an object")
    if sampling.get("measurement_kind") != "sampled_process_tree":
        raise ValueError("sampling.measurement_kind must be sampled_process_tree")
    interval_ms = sampling.get("interval_ms")
    if not isinstance(interval_ms, int) or isinstance(interval_ms, bool) or interval_ms <= 0:
        raise ValueError("sampling.interval_ms must be a positive integer")
    limitations = sampling.get("limitations")
    if not isinstance(limitations, list) or SAMPLED_TREE_LIMITATION not in limitations:
        raise ValueError("sampling.limitations must disclose sub-interval descendant loss")
    traffic_profiles = document.get("traffic_profiles")
    if not isinstance(traffic_profiles, dict) or not traffic_profiles:
        raise ValueError("traffic_profiles must be a non-empty object")
    for profile_name, profile in traffic_profiles.items():
        if not isinstance(profile_name, str) or not isinstance(profile, dict):
            raise ValueError("traffic profile names and values must be objects")
        for field in (
            "throttle_bps",
            "outage_every_bytes",
            "outage_ms",
            "disconnect_every_bytes",
        ):
            value = profile.get(field)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError(f"traffic_profiles.{profile_name}.{field} must be non-negative")
    fixture = document.get("fixture")
    if not isinstance(fixture, dict):
        raise ValueError("fixture must be an object")
    for field in ("duration_s", "sample_rate_hz", "margin_s"):
        value = fixture.get(field)
        if not scenario_finite_number(value) or float(value) <= 0:
            raise ValueError(f"fixture.{field} must be finite and positive")
    scenarios = document.get("scenarios")
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError("scenarios must be a non-empty list")
    seen: set[str] = set()
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            raise ValueError("each scenario must be an object")
        name = scenario.get("id")
        if not isinstance(name, str) or not name or name in seen:
            raise ValueError(f"invalid or duplicate scenario id: {name!r}")
        seen.add(name)
        pairs = scenario.get("pairs")
        if not isinstance(pairs, int) or isinstance(pairs, bool) or pairs < 1:
            raise ValueError(f"{name}.pairs must be a positive integer")
        min_improved_pairs = scenario.get("min_improved_pairs")
        if "min_improved_pairs" in scenario and (
            not isinstance(min_improved_pairs, int)
            or isinstance(min_improved_pairs, bool)
            or not 0 <= min_improved_pairs <= pairs
        ):
            raise ValueError(
                f"{name}.min_improved_pairs must be an integer in [0,{pairs}]"
            )
        repeats = scenario.get("candidate_repeats")
        if not isinstance(repeats, int) or isinstance(repeats, bool) or repeats < 0:
            raise ValueError(f"{name}.candidate_repeats must be a non-negative integer")
        for field in ("warmup_s", "sample_s"):
            value = scenario.get(field)
            if (
                not scenario_finite_number(value)
                or float(value) < 0
            ):
                raise ValueError(f"{name}.{field} must be finite and non-negative")
        if name != "render_and_interaction" and float(scenario["sample_s"]) <= 0:
            raise ValueError(f"{name}.sample_s must be positive for process sampling")
        geometry = scenario.get("geometry")
        if not (
            isinstance(geometry, list)
            and geometry
            and all(
                isinstance(item, list)
                and len(item) == 2
                and all(
                    isinstance(value, int)
                    and not isinstance(value, bool)
                    and value > 0
                    for value in item
                )
                for item in geometry
            )
        ):
            raise ValueError(f"{name}.geometry must contain positive [width,height] pairs")
        if len({tuple(item) for item in geometry}) != len(geometry):
            raise ValueError(f"{name}.geometry must not contain duplicate pairs")
        profile = scenario.get("traffic_profile")
        if profile not in traffic_profiles:
            raise ValueError(f"{name}.traffic_profile references unknown profile {profile!r}")
        requires_mpv = scenario.get("requires_mpv")
        if not isinstance(requires_mpv, bool):
            raise ValueError(f"{name}.requires_mpv must be boolean")
        controller = scenario.get("controller")
        if not isinstance(controller, bool):
            raise ValueError(f"{name}.controller must be boolean")
        pause_policy = scenario.get("pause_policy")
        if pause_policy not in {"none", "pause-resume"}:
            raise ValueError(f"{name}.pause_policy must be none or pause-resume")
        pause_hold_ms = scenario.get("pause_hold_ms")
        if not isinstance(pause_hold_ms, int) or isinstance(pause_hold_ms, bool):
            raise ValueError(f"{name}.pause_hold_ms must be an integer")
        if pause_policy == "none" and pause_hold_ms != 0:
            raise ValueError(f"{name}.pause_policy none requires pause_hold_ms=0")
        if pause_policy == "pause-resume":
            if not controller:
                raise ValueError(f"{name}.pause-resume requires controller=true")
            if pause_hold_ms <= 0:
                raise ValueError(f"{name}.pause-resume requires positive pause_hold_ms")
        if controller and not requires_mpv:
            raise ValueError(f"{name}.controller requires requires_mpv=true")
        if controller:
            if scenario.get("controller_load") not in {"none", "resume-session"}:
                raise ValueError(f"{name}.controller_load is invalid")
            seeks = scenario.get("seeks_s")
            if not (
                isinstance(seeks, list)
                and all(
                    scenario_finite_number(value)
                    and float(value) >= 0
                    for value in seeks
                )
            ):
                raise ValueError(f"{name}.seeks_s must contain finite non-negative numbers")
        steady_playback = (
            controller
            and requires_mpv
            and pause_policy == "none"
            and scenario.get("seeks_s") == []
        )
        progress_fields = (
            "minimum_playback_progress_fraction",
            "time_pos_tail_tolerance_s",
        )
        if steady_playback:
            progress_fraction = scenario.get("minimum_playback_progress_fraction")
            tail_tolerance = scenario.get("time_pos_tail_tolerance_s")
            if (
                not scenario_finite_number(progress_fraction)
                or not 0 < float(progress_fraction) <= 1
            ):
                raise ValueError(
                    f"{name}.minimum_playback_progress_fraction must be in (0,1]"
                )
            if (
                not scenario_finite_number(tail_tolerance)
                or float(tail_tolerance) <= 0
            ):
                raise ValueError(f"{name}.time_pos_tail_tolerance_s must be positive")
        elif any(field in scenario for field in progress_fields):
            raise ValueError(
                f"{name} may declare steady playback progress only for no-seek, "
                "no-pause playback"
            )
        if requires_mpv:
            expected_mpv_cache = scenario.get("expected_effective_mpv_cache_args")
            if expected_mpv_cache != REQUIRED_PLAYBACK_MPV_CACHE_ARGS:
                raise ValueError(
                    f"{name}.expected_effective_mpv_cache_args must declare the exact "
                    "32MiB/8MiB effective mpv cache argv contract"
                )
            seed_contract = scenario.get("seed_contract")
            if not isinstance(seed_contract, dict):
                raise ValueError(f"{name}.seed_contract must be an object")
            if seed_contract.get("require_identical_tree") is not True:
                raise ValueError(f"{name}.seed_contract must require identical trees")
            if seed_contract.get("require_identical_cache_policy") is not True:
                raise ValueError(f"{name}.seed_contract must require identical cache policy")
            expected_cache = seed_contract.get("expected_cache_policy")
            if expected_cache is not None and not isinstance(expected_cache, dict):
                raise ValueError(f"{name}.seed_contract.expected_cache_policy must be object or null")
            margin = float(fixture["margin_s"])
            observation_end = float(scenario["warmup_s"]) + float(scenario["sample_s"])
            furthest_seek = max((float(value) for value in scenario.get("seeks_s", [])), default=0.0)
            required_duration = max(observation_end, furthest_seek) + margin
            if float(fixture["duration_s"]) < required_duration:
                raise ValueError(
                    f"fixture.duration_s must be at least {required_duration:g} for {name}"
                )
        elif "expected_effective_mpv_cache_args" in scenario:
            raise ValueError(
                f"{name}.expected_effective_mpv_cache_args requires requires_mpv=true"
            )
        metrics = scenario.get("metrics")
        if not isinstance(metrics, dict) or not metrics:
            raise ValueError(f"{name}.metrics must be a non-empty object")
        for metric, policy in metrics.items():
            if not isinstance(metric, str) or not metric or not isinstance(policy, dict):
                raise ValueError(f"{name}.metrics entries must be object policies")
            comparison = policy.get("comparison", "ratio")
            if comparison not in {"ratio", "latency", "no_increase", "exact"}:
                raise ValueError(f"{name}.{metric}: unsupported comparison {comparison!r}")
            for threshold in ("max_ratio", "max_delta"):
                if threshold not in policy:
                    continue
                value = policy[threshold]
                if not scenario_finite_number(value):
                    raise ValueError(
                        f"{name}.{metric}: {threshold} must be a finite number"
                    )
            if comparison in {"ratio", "latency"}:
                max_ratio = policy.get("max_ratio")
                if not isinstance(max_ratio, (int, float)) or isinstance(max_ratio, bool):
                    raise ValueError(f"{name}.{metric}: ratio policy needs max_ratio")
                if float(max_ratio) < 0:
                    raise ValueError(f"{name}.{metric}: max_ratio must be non-negative")
            if comparison == "latency" and "max_delta" not in policy:
                raise ValueError(f"{name}.{metric}: latency policy needs max_delta")
            metric_min_improved = policy.get("min_improved_pairs")
            if "min_improved_pairs" in policy and (
                not isinstance(metric_min_improved, int)
                or isinstance(metric_min_improved, bool)
                or not 0 <= metric_min_improved <= pairs
            ):
                raise ValueError(
                    f"{name}.{metric}: min_improved_pairs must be an integer in [0,{pairs}]"
                )


def scenario_validation_self_test() -> None:
    document, _digest = load_scenarios(DEFAULT_SCENARIOS)

    def first_metric(scenario_document: dict[str, Any]) -> dict[str, Any]:
        return next(iter(scenario_document["scenarios"][0]["metrics"].values()))

    mutations: list[tuple[str, Any]] = [
        (
            "boolean bootstrap resamples",
            lambda value: value["statistics"].__setitem__("bootstrap_resamples", True),
        ),
        (
            "floating bootstrap resamples",
            lambda value: value["statistics"].__setitem__("bootstrap_resamples", 50_000.0),
        ),
        (
            "zero bootstrap resamples",
            lambda value: value["statistics"].__setitem__("bootstrap_resamples", 0),
        ),
        (
            "excessive bootstrap resamples",
            lambda value: value["statistics"].__setitem__(
                "bootstrap_resamples", MAX_BOOTSTRAP_RESAMPLES + 1
            ),
        ),
        (
            "boolean statistics seed",
            lambda value: value["statistics"].__setitem__("seed", False),
        ),
        (
            "negative statistics seed",
            lambda value: value["statistics"].__setitem__("seed", -1),
        ),
        (
            "oversized statistics seed",
            lambda value: value["statistics"].__setitem__(
                "seed", MAX_STATISTICS_SEED + 1
            ),
        ),
        (
            "non-finite confidence",
            lambda value: value["statistics"].__setitem__(
                "one_sided_confidence", math.nan
            ),
        ),
        (
            "zero confidence",
            lambda value: value["statistics"].__setitem__("one_sided_confidence", 0),
        ),
        (
            "unit confidence",
            lambda value: value["statistics"].__setitem__("one_sided_confidence", 1),
        ),
        (
            "non-finite CV limit",
            lambda value: value["statistics"].__setitem__("baseline_cv_max", math.inf),
        ),
        (
            "negative CV limit",
            lambda value: value["statistics"].__setitem__("baseline_cv_max", -0.1),
        ),
        (
            "non-finite fixture duration",
            lambda value: value["fixture"].__setitem__("duration_s", math.nan),
        ),
        (
            "non-finite fixture sample rate",
            lambda value: value["fixture"].__setitem__("sample_rate_hz", math.inf),
        ),
        (
            "overflowing fixture margin",
            lambda value: value["fixture"].__setitem__("margin_s", 10**1000),
        ),
        (
            "overflowing warmup",
            lambda value: value["scenarios"][0].__setitem__("warmup_s", 10**1000),
        ),
        (
            "non-finite sample duration",
            lambda value: value["scenarios"][0].__setitem__("sample_s", math.inf),
        ),
        (
            "overflowing seek target",
            lambda value: value["scenarios"][1]["seeks_s"].__setitem__(0, 10**1000),
        ),
        (
            "overflowing steady progress fraction",
            lambda value: value["scenarios"][5].__setitem__(
                "minimum_playback_progress_fraction", 10**1000
            ),
        ),
        (
            "non-finite steady tail tolerance",
            lambda value: value["scenarios"][5].__setitem__(
                "time_pos_tail_tolerance_s", math.inf
            ),
        ),
        (
            "boolean scenario improved-pair count",
            lambda value: value["scenarios"][0].__setitem__("min_improved_pairs", True),
        ),
        (
            "null scenario improved-pair count",
            lambda value: value["scenarios"][0].__setitem__("min_improved_pairs", None),
        ),
        (
            "scenario improved-pair count above pairs",
            lambda value: value["scenarios"][0].__setitem__(
                "min_improved_pairs", value["scenarios"][0]["pairs"] + 1
            ),
        ),
        (
            "boolean metric improved-pair count",
            lambda value: first_metric(value).__setitem__("min_improved_pairs", False),
        ),
        (
            "null metric improved-pair count",
            lambda value: first_metric(value).__setitem__("min_improved_pairs", None),
        ),
        (
            "metric improved-pair count above pairs",
            lambda value: first_metric(value).__setitem__(
                "min_improved_pairs", value["scenarios"][0]["pairs"] + 1
            ),
        ),
        (
            "non-finite ratio threshold",
            lambda value: first_metric(value).__setitem__("max_ratio", math.nan),
        ),
        (
            "boolean ratio threshold",
            lambda value: first_metric(value).__setitem__("max_ratio", True),
        ),
        (
            "negative ratio threshold",
            lambda value: first_metric(value).__setitem__("max_ratio", -0.1),
        ),
        (
            "non-finite latency delta threshold",
            lambda value: value["scenarios"][1]["metrics"][
                "operation.resume_session.p95_ms"
            ].__setitem__("max_delta", math.inf),
        ),
        (
            "non-finite no-increase delta threshold",
            lambda value: value["scenarios"][1]["metrics"][
                "buffering_events"
            ].__setitem__("max_delta", math.nan),
        ),
        (
            "duplicate geometry",
            lambda value: value["scenarios"][0]["geometry"].append([80, 24]),
        ),
        (
            "boolean geometry component",
            lambda value: value["scenarios"][0]["geometry"][0].__setitem__(0, True),
        ),
    ]
    for label, mutate in mutations:
        invalid = json.loads(json.dumps(document))
        mutate(invalid)
        try:
            validate_scenarios(invalid)
        except ValueError:
            continue
        raise AssertionError(f"scenario validation accepted {label}")


def find_scenario(document: dict[str, Any], name: str) -> dict[str, Any]:
    for scenario in document["scenarios"]:
        if scenario["id"] == name:
            return scenario
    raise ValueError(f"unknown scenario {name!r}")


def dotted(value: Any, path: str) -> Any:
    current = value
    for part in path.split("."):
        if isinstance(current, dict) and part in current:
            current = current[part]
        elif isinstance(current, list) and part == "length":
            current = len(current)
        elif isinstance(current, list) and part.isdigit() and int(part) < len(current):
            current = current[int(part)]
        else:
            raise ValueError(f"field {path!r} not found (missing {part!r})")
    return current


def command_validate(args: argparse.Namespace) -> int:
    document, digest = load_scenarios(args.scenarios)
    print(json.dumps({"ok": True, "sha256": digest, "scenario_count": len(document["scenarios"])}))
    return 0


def command_scenario(args: argparse.Namespace) -> int:
    document, digest = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.id)
    if args.field == "sha256":
        print(digest)
    elif args.field:
        value = dotted(scenario, args.field)
        if isinstance(value, (dict, list)):
            print(json.dumps(value, separators=(",", ":")))
        elif isinstance(value, bool):
            print("true" if value else "false")
        else:
            print(value)
    else:
        print(json.dumps({"scenario_sha256": digest, **scenario}, indent=2, sort_keys=True))
    return 0


def command_traffic(args: argparse.Namespace) -> int:
    document, _ = load_scenarios(args.scenarios)
    try:
        profile = document["traffic_profiles"][args.name]
    except KeyError as error:
        raise ValueError(f"unknown traffic profile {args.name!r}") from error
    value = dotted(profile, args.field) if args.field else profile
    if isinstance(value, (dict, list)):
        print(json.dumps(value, separators=(",", ":")))
    else:
        print(value)
    return 0


def command_setting(args: argparse.Namespace) -> int:
    document, _ = load_scenarios(args.scenarios)
    value = dotted(document, args.field)
    if isinstance(value, (dict, list)):
        print(json.dumps(value, separators=(",", ":")))
    elif isinstance(value, bool):
        print("true" if value else "false")
    else:
        print(value)
    return 0


def command_run_start(args: argparse.Namespace) -> int:
    if args.output.exists():
        raise ValueError("run contract output must name a new path")
    document, scenario_hash = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.scenario)
    pairs = int(scenario["pairs"])
    repeats = int(scenario.get("candidate_repeats", 0))
    if args.kind == "paired":
        if args.pair_index is None or not 1 <= args.pair_index <= pairs:
            raise ValueError(f"paired run requires --pair-index in 1..{pairs}")
        if args.repeat_index is not None:
            raise ValueError("paired run must not set --repeat-index")
        order = (
            ["baseline", "candidate"]
            if args.pair_index % 2 == 1
            else ["candidate", "baseline"]
        )
        ordinal = order.index(args.role) + 1
        pair_index = args.pair_index
        repeat_index = None
    else:
        if args.role != "candidate":
            raise ValueError("candidate_repeat run role must be candidate")
        if args.repeat_index is None or not 1 <= args.repeat_index <= repeats:
            raise ValueError(f"candidate_repeat requires --repeat-index in 1..{repeats}")
        if args.pair_index is not None:
            raise ValueError("candidate_repeat must not set --pair-index")
        order = None
        ordinal = None
        pair_index = None
        repeat_index = args.repeat_index
    geometry_index = getattr(args, "geometry_index", None)
    requested_width = getattr(args, "width", None)
    requested_height = getattr(args, "height", None)
    terminal_geometry = None
    if geometry_index is not None:
        if scenario["id"] == "render_and_interaction" or len(scenario["geometry"]) <= 1:
            raise ValueError("per-geometry contracts are only valid for multi-geometry process runs")
        if not 0 <= geometry_index < len(scenario["geometry"]):
            raise ValueError(
                f"--geometry-index must be in 0..{len(scenario['geometry']) - 1}"
            )
        terminal_geometry = [requested_width, requested_height]
        require_artifact_value(
            args.output,
            "requested terminal geometry",
            terminal_geometry,
            scenario["geometry"][geometry_index],
        )
    elif requested_width is not None or requested_height is not None:
        raise ValueError("--width/--height require --geometry-index")
    elif scenario["id"] != "render_and_interaction" and len(scenario["geometry"]) == 1:
        terminal_geometry = scenario["geometry"][0]
    host_identity = stable_host_identity()
    started_unix_ns = time.time_ns()
    started_monotonic_ns = time.monotonic_ns()
    run_id = (
        f"{args.scenario}:{args.kind}:{args.role}:"
        f"{pair_index if pair_index is not None else repeat_index}:"
        f"{secrets.token_hex(16)}"
    )
    contract = {
        "schema": RUN_CONTRACT_SCHEMA,
        "state": "running",
        "run_id": run_id,
        "scenario": args.scenario,
        "scenario_sha256": scenario_hash,
        "kind": args.kind,
        "role": args.role,
        "pair_index": pair_index,
        "pair_order": order,
        "within_pair_ordinal": ordinal,
        "repeat_index": repeat_index,
        "geometry": scenario["geometry"],
        "geometry_index": geometry_index,
        "terminal_geometry": terminal_geometry,
        "started_unix_ns": started_unix_ns,
        "started_monotonic_ns": started_monotonic_ns,
        "monotonic_clock": time.get_clock_info("monotonic").implementation,
        "host": host_identity,
    }
    atomic_json(args.output, contract)
    print(run_id)
    return 0


def command_run_finish(args: argparse.Namespace) -> int:
    contract = load_json_object(args.contract)
    require_artifact_value(args.contract, "schema", contract.get("schema"), RUN_CONTRACT_SCHEMA)
    require_artifact_value(args.contract, "state", contract.get("state"), "running")
    require_artifact_value(
        args.contract, "current host identity", contract.get("host"), stable_host_identity()
    )
    started_monotonic_ns = non_negative_integer(
        contract.get("started_monotonic_ns"), "started_monotonic_ns", args.contract
    )
    finished_monotonic_ns = time.monotonic_ns()
    finished_unix_ns = time.time_ns()
    if finished_monotonic_ns <= started_monotonic_ns:
        raise ValueError(f"{args.contract}: run finish must follow run start")
    finished = {
        **contract,
        "state": "finished",
        "finished_unix_ns": finished_unix_ns,
        "finished_monotonic_ns": finished_monotonic_ns,
        "duration_ns": finished_monotonic_ns - started_monotonic_ns,
    }
    atomic_json(args.contract, finished)
    print(json.dumps({"ok": True, "run_id": finished["run_id"]}))
    return 0


def package_version_from_cargo_toml(cargo_toml: Path) -> str:
    match = re.search(
        r"(?ms)^\[package\]\s*.*?^version\s*=\s*\"([^\"]+)\"",
        cargo_toml.read_text(encoding="utf-8"),
    )
    if not match:
        raise ValueError(f"cannot determine package version from {cargo_toml}")
    return match.group(1)


def project_package_version() -> str:
    return package_version_from_cargo_toml(
        Path(__file__).resolve().parent.parent / "Cargo.toml"
    )


def playlist_marker_occurrences(root: Path, marker: str) -> int:
    count = 0
    for path in sorted(item for item in root.rglob("*.json") if item.is_file()):
        try:
            count += path.read_text(encoding="utf-8").count(marker)
        except (OSError, UnicodeDecodeError):
            continue
    return count


def validate_active_session_playlist(root: Path, expected_local_path: str) -> dict[str, Any]:
    session = root / "stores" / "cache" / "session.json"
    journal = session.with_name("session.json.intent.jsonl")
    sidecar = session.with_name("session.json.intent.latest.json")
    replay_inputs = [path.relative_to(root).as_posix() for path in (journal, sidecar) if path.exists()]
    if replay_inputs:
        raise ValueError(
            "performance seeds must not contain session intent replay files: "
            f"{replay_inputs}"
        )
    if not session.is_file():
        raise ValueError("seed must contain exact stores/cache/session.json")
    try:
        document = json.loads(
            session.read_text(encoding="utf-8"),
            object_pairs_hook=reject_duplicate_json_keys,
        )
    except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(f"invalid session cache {session}: {error}") from error
    if not isinstance(document, dict):
        raise ValueError(f"{session}: session cache must be an object")
    require_artifact_value(session, "session schema", document.get("schema_version"), 2)
    require_artifact_value(
        session, "session app version", document.get("app_version"), project_package_version()
    )
    last_mode = document.get("last_mode")
    queue_field = {"normal": "normal_queue", "radio": "radio_queue", "local": "local_queue"}.get(last_mode)
    if queue_field is None:
        raise ValueError(f"{session}: last_mode must select normal, radio, or local")
    queue = document.get(queue_field)
    if not isinstance(queue, dict):
        raise ValueError(f"{session}: active {queue_field} must be an object")
    songs = queue.get("songs")
    order = queue.get("order")
    cursor = queue.get("cursor")
    if not isinstance(songs, list) or not 1 <= len(songs) <= 999:
        raise ValueError(f"{session}: active songs must contain 1..999 entries")
    if (
        not isinstance(order, list)
        or any(not isinstance(value, int) or isinstance(value, bool) for value in order)
        or sorted(order) != list(range(len(songs)))
    ):
        raise ValueError(f"{session}: active order must be an exact song-index permutation")
    if not isinstance(cursor, int) or isinstance(cursor, bool) or not 0 <= cursor < len(order):
        raise ValueError(f"{session}: active cursor is outside the queue order")
    current_song_index = order[cursor]
    current = songs[current_song_index]
    if not isinstance(current, dict):
        raise ValueError(f"{session}: active current song must be an object")
    require_artifact_value(
        session,
        "active current Song.local_path",
        current.get("local_path"),
        expected_local_path,
    )
    occurrences = playlist_marker_occurrences(root, expected_local_path)
    require_artifact_value(
        session,
        f"total JSON occurrence count for {expected_local_path!r}",
        occurrences,
        1,
    )
    return {
        "session": session.relative_to(root).as_posix(),
        "schema_version": 2,
        "app_version": project_package_version(),
        "last_mode": last_mode,
        "active_queue": queue_field,
        "cursor": cursor,
        "current_song_index": current_song_index,
        "local_path": expected_local_path,
        "total_json_occurrences": occurrences,
        "intent_replay_files": [],
    }


def deterministic_fixture_song(local_path: str) -> dict[str, Any]:
    return {
        "video_id": "tui-perf-fixture",
        "title": "Deterministic TUI Performance Fixture",
        "artist": "ytm-tui perf harness",
        "duration": "18:00",
        "duration_secs": 1080,
        "local_path": local_path,
    }


def materialize_single_song_active_session(root: Path, local_path: str) -> Path:
    session = root / "stores" / "cache" / "session.json"
    document = load_json_object(session)
    queue_field = {
        "normal": "normal_queue",
        "radio": "radio_queue",
        "local": "local_queue",
    }.get(document.get("last_mode"))
    if queue_field is None:
        raise ValueError(f"{session}: last_mode does not identify an active queue")
    document[queue_field] = {
        "songs": [deterministic_fixture_song(local_path)],
        "order": [0],
        "cursor": 0,
        "shuffle": False,
        "repeat": "off",
    }
    for inactive in {"normal_queue", "radio_queue", "local_queue"} - {queue_field}:
        document[inactive] = None
    atomic_json(session, document)
    return session


def validate_materialized_active_session_playlist(
    root: Path, expected_local_path: str
) -> dict[str, Any]:
    contract = validate_active_session_playlist(root, expected_local_path)
    session = root / "stores" / "cache" / "session.json"
    document = load_json_object(session)
    queue_field = contract["active_queue"]
    expected_queue = {
        "songs": [deterministic_fixture_song(expected_local_path)],
        "order": [0],
        "cursor": 0,
        "shuffle": False,
        "repeat": "off",
    }
    require_artifact_value(
        session, "deterministic single-song active queue", document.get(queue_field), expected_queue
    )
    inactive = {
        field: document.get(field)
        for field in ("normal_queue", "radio_queue", "local_queue")
        if field != queue_field
    }
    require_artifact_value(
        session,
        "inactive queue isolation",
        inactive,
        {field: None for field in inactive},
    )
    return {
        **contract,
        "deterministic_single_song": True,
        "inactive_queues_cleared": True,
    }


def materialized_session_self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="ytt-perf-session-self-test-") as temporary:
        root = Path(temporary)
        session = root / "stores" / "cache" / "session.json"
        session.parent.mkdir(parents=True)
        remote = "https://remote.example.invalid/watch?v=must-not-survive"
        atomic_json(
            session,
            {
                "schema_version": 2,
                "app_version": project_package_version(),
                "last_mode": "normal",
                "normal_queue": {
                    "songs": [
                        {"title": "seed current", "local_path": "{{TUI_PERF_PLAYLIST}}"},
                        {
                            "title": "hostile remote next track",
                            "local_path": remote,
                            "thumbnails": [{"url": remote}],
                        },
                    ],
                    "order": [0, 1],
                    "cursor": 0,
                    "shuffle": True,
                    "repeat": "all",
                },
                "radio_queue": {
                    "songs": [{"title": "inactive remote", "local_path": remote}],
                    "order": [0],
                    "cursor": 0,
                },
                "local_queue": None,
            },
        )
        validate_active_session_playlist(root, "{{TUI_PERF_PLAYLIST}}")
        local_path = str(root / "fixture.m3u8")
        materialize_single_song_active_session(root, local_path)
        validate_materialized_active_session_playlist(root, local_path)
        if remote in session.read_text(encoding="utf-8"):
            raise AssertionError("materialized session retained a remote queue target")

        tampered = load_json_object(session)
        active = tampered["normal_queue"]
        active["songs"].append({"title": "tampered remote", "local_path": remote})
        active["order"].append(1)
        atomic_json(session, tampered)
        try:
            validate_materialized_active_session_playlist(root, local_path)
        except ValueError:
            pass
        else:
            raise AssertionError("materialized session accepted a hostile second track")


def byte_exact_tree_state_for_self_test(
    root: Path,
) -> tuple[tuple[str, str, bytes | None], ...]:
    resolved = root.resolve()
    entries: list[tuple[str, str, bytes | None]] = [(".", "directory", None)]
    for path in sorted(
        resolved.rglob("*"), key=lambda item: item.relative_to(resolved).as_posix()
    ):
        relative = path.relative_to(resolved).as_posix()
        metadata = path.lstat()
        if path.is_symlink() or bool(getattr(path, "is_junction", lambda: False)()):
            raise AssertionError(f"self-test input tree unexpectedly contains a link: {path}")
        if stat.S_ISDIR(metadata.st_mode):
            entries.append((relative, "directory", None))
        elif stat.S_ISREG(metadata.st_mode):
            entries.append((relative, "file", path.read_bytes()))
        else:
            raise AssertionError(f"self-test input tree contains a special entry: {path}")
    return tuple(entries)


def materialize_command_self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="ytt-perf-materialize-self-test-") as temporary:
        base = Path(temporary)
        home = base / "home"
        for store in ("config", "data", "cache"):
            (home / "stores" / store).mkdir(parents=True)
        atomic_json(home / "stores" / "config" / "config.json", {})
        remote = "https://remote.example.invalid/hostile-next-track"
        session = home / "stores" / "cache" / "session.json"
        atomic_json(
            session,
            {
                "schema_version": 2,
                "app_version": project_package_version(),
                "last_mode": "normal",
                "normal_queue": {
                    "songs": [
                        {"title": "seed current", "local_path": "{{TUI_PERF_PLAYLIST}}"},
                        {"title": "hostile next", "local_path": remote},
                    ],
                    "order": [0, 1],
                    "cursor": 0,
                },
                "radio_queue": {
                    "songs": [{"title": "hostile inactive", "local_path": remote}],
                    "order": [0],
                    "cursor": 0,
                },
                "local_queue": None,
            },
        )

        def expect_path_rejected(
            label: str, manifest_path: Path, input_snapshot: Path
        ) -> None:
            before = byte_exact_tree_state_for_self_test(home)
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    command_materialize(
                        argparse.Namespace(
                            root=home,
                            home=home,
                            fixture_url="http://127.0.0.1:8123/fixture.wav",
                            playlist_relative=Path("fixture/tui-perf-stream.m3u"),
                            manifest=manifest_path,
                            input_snapshot=input_snapshot,
                            seed_label=f"rejected-{label}",
                        )
                    )
            except ValueError:
                pass
            else:
                raise AssertionError(f"materialize accepted invalid {label} paths")
            if byte_exact_tree_state_for_self_test(home) != before:
                raise AssertionError(
                    f"materialize invalid {label} paths changed input-tree bytes or shape"
                )

        contained_outputs = home / "rejected-materialize-outputs"
        expect_path_rejected(
            "contained",
            contained_outputs / "materialize.json",
            contained_outputs / "materialized-inputs",
        )

        occupied_parent = base / "rejected-occupied-snapshot"
        occupied_parent.mkdir()
        occupied_snapshot = occupied_parent / "materialized-inputs"
        occupied_snapshot.mkdir()
        expect_path_rejected(
            "occupied-snapshot",
            occupied_parent / "materialize.json",
            occupied_snapshot,
        )

        occupied_manifest_parent = base / "rejected-occupied-manifest"
        occupied_manifest_parent.mkdir()
        occupied_manifest = occupied_manifest_parent / "materialize.json"
        atomic_text(occupied_manifest, "do-not-overwrite\n")
        expect_path_rejected(
            "occupied-manifest",
            occupied_manifest,
            occupied_manifest_parent / "materialized-inputs",
        )
        if occupied_manifest.read_text(encoding="utf-8") != "do-not-overwrite\n":
            raise AssertionError("rejected materialize overwrote an occupied manifest")

        mismatched_manifest_parent = base / "rejected-manifest-parent"
        mismatched_snapshot_parent = base / "rejected-snapshot-parent"
        mismatched_manifest_parent.mkdir()
        mismatched_snapshot_parent.mkdir()
        expect_path_rejected(
            "mismatched-parent",
            mismatched_manifest_parent / "materialize.json",
            mismatched_snapshot_parent / "materialized-inputs",
        )

        evidence = base / "evidence"
        evidence.mkdir()
        manifest_path = evidence / "materialize.json"
        input_snapshot = evidence / "materialized-inputs"
        with contextlib.redirect_stdout(io.StringIO()):
            command_materialize(
                argparse.Namespace(
                    root=home,
                    home=home,
                    fixture_url="http://127.0.0.1:8123/fixture.wav",
                    playlist_relative=Path("fixture/tui-perf-stream.m3u"),
                    manifest=manifest_path,
                    input_snapshot=input_snapshot,
                    seed_label="hostile-self-test",
                )
            )
        manifest = load_json_object(manifest_path)
        expected_path = str(home.resolve() / "fixture" / "tui-perf-stream.m3u")
        live_contract = validate_materialized_active_session_playlist(home.resolve(), expected_path)
        snapshot_contract = validate_materialized_active_session_playlist(
            input_snapshot, expected_path
        )
        require_artifact_value(
            manifest_path,
            "materialized active playlist contract",
            manifest.get("materialized_active_playlist_contract"),
            live_contract,
        )
        require_artifact_value(
            manifest_path,
            "materialized snapshot active playlist contract",
            snapshot_contract,
            live_contract,
        )
        require_artifact_value(
            manifest_path,
            "materialized changed path inventory",
            manifest.get("changed"),
            ["fixture/tui-perf-stream.m3u", "stores/cache/session.json"],
        )
        for materialized_session in (
            session,
            input_snapshot / "stores" / "cache" / "session.json",
        ):
            if remote in materialized_session.read_text(encoding="utf-8"):
                raise AssertionError("materialize command retained a hostile remote queue target")

        tampered = load_json_object(input_snapshot / "stores" / "cache" / "session.json")
        tampered["normal_queue"]["songs"].append(
            {"title": "tampered next", "local_path": remote}
        )
        tampered["normal_queue"]["order"].append(1)
        atomic_json(input_snapshot / "stores" / "cache" / "session.json", tampered)
        try:
            validate_materialized_active_session_playlist(input_snapshot, expected_path)
        except ValueError:
            pass
        else:
            raise AssertionError("materialized snapshot accepted a hostile second track")


def reject_seed_symlinks(root: Path) -> None:
    links = sorted(path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_symlink())
    if links:
        raise ValueError(f"seed templates must not contain symlinks: {links}")


def resolved_paths_overlap(first: Path, second: Path) -> bool:
    try:
        first.relative_to(second)
        return True
    except ValueError:
        pass
    try:
        second.relative_to(first)
        return True
    except ValueError:
        return False


def path_entry_exists(path: Path) -> bool:
    try:
        path.lstat()
    except FileNotFoundError:
        return False
    except OSError as error:
        raise ValueError(f"cannot inspect path {path}: {error}") from error
    return True


def reject_output_root_overlap(output_root: Path, protected_roots: list[Path]) -> Path:
    resolved_output = output_root.resolve()
    resolved_roots = [root.resolve() for root in protected_roots]
    for index, root in enumerate(resolved_roots):
        if not root.is_dir():
            raise ValueError(f"protected root {index} does not exist: {root}")
        if resolved_paths_overlap(resolved_output, root):
            raise ValueError(
                "new evidence output root must not equal, contain, or be contained by "
                f"protected root {index}: output={resolved_output}, root={root}"
            )
    if path_entry_exists(output_root) or path_entry_exists(resolved_output):
        raise ValueError(f"evidence output root must name a new path: {resolved_output}")
    return resolved_output


def command_path_preflight(args: argparse.Namespace) -> int:
    output_root = reject_output_root_overlap(args.output_root, args.protected_root)
    print(output_root)
    return 0


def resolve_seed_contract_paths(
    args: argparse.Namespace,
) -> tuple[dict[str, Path], Path, Path, Path]:
    roots = {
        "baseline": args.baseline_root.resolve(),
        "candidate": args.candidate_root.resolve(),
    }
    output = args.output.resolve()
    evidence_root = output.parent
    snapshot = args.snapshot.resolve()
    if snapshot.parent != evidence_root:
        raise ValueError("seed snapshot and contract output must be direct siblings in one evidence root")
    if snapshot == output:
        raise ValueError("seed snapshot and contract output must be distinct paths")
    protected_outputs = {
        "evidence root": evidence_root,
        "snapshot": snapshot,
        "contract output": output,
    }
    for role, root in roots.items():
        for label, protected in protected_outputs.items():
            if resolved_paths_overlap(protected, root):
                raise ValueError(
                    f"{label} must not equal, contain, or be contained by {role} seed root: "
                    f"{protected} versus {root}"
                )
    if path_entry_exists(args.output) or path_entry_exists(output):
        raise ValueError("seed contract output must name a new path")
    if path_entry_exists(args.snapshot) or path_entry_exists(snapshot):
        raise ValueError("--snapshot must name a new path")
    return roots, output, evidence_root, snapshot


def command_seed_contract(args: argparse.Namespace) -> int:
    roots, output, evidence_root, snapshot = resolve_seed_contract_paths(args)
    document, scenario_hash = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.scenario)
    if not scenario["requires_mpv"]:
        raise ValueError("seed-contract is only valid for playback scenarios")
    for role, root in roots.items():
        if not root.is_dir():
            raise ValueError(f"{role} seed root does not exist: {root}")
        reject_seed_symlinks(root)
        for store in ("config", "data", "cache"):
            if not (root / "stores" / store).is_dir():
                raise ValueError(f"{role} seed root is missing stores/{store}")
    tree_hashes = {role: sha256_tree(root) for role, root in roots.items()}
    policies = {role: seed_cache_policy(root) for role, root in roots.items()}
    if len(set(tree_hashes.values())) != 1:
        raise ValueError(f"baseline/candidate seed tree SHA-256 differs: {tree_hashes}")
    if policies["baseline"] != policies["candidate"]:
        raise ValueError("baseline/candidate seed cache policy differs")
    expected = scenario["seed_contract"].get("expected_cache_policy")
    if expected is not None and policies["baseline"] != expected:
        raise ValueError(
            f"seed cache policy {policies['baseline']!r} does not match scenario contract {expected!r}"
        )
    playlist_contracts = {
        role: validate_active_session_playlist(root, "{{TUI_PERF_PLAYLIST}}")
        for role, root in roots.items()
    }
    placeholders = {
        role: contract["total_json_occurrences"]
        for role, contract in playlist_contracts.items()
    }

    snapshot.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(roots["baseline"], snapshot)
    reject_seed_symlinks(snapshot)
    snapshot_hash = sha256_tree(snapshot)
    if snapshot_hash != tree_hashes["baseline"]:
        raise ValueError("copied seed snapshot does not match the validated seed tree")
    try:
        snapshot_relative = snapshot.relative_to(evidence_root).as_posix()
    except ValueError as error:
        raise ValueError("seed snapshot must stay inside the evidence root") from error
    manifest = {
        "schema": SEED_CONTRACT_SCHEMA,
        "scenario": scenario["id"],
        "scenario_sha256": scenario_hash,
        "contract": scenario["seed_contract"],
        "source_roots": {role: str(root) for role, root in roots.items()},
        "source_tree_sha256": tree_hashes,
        "cache_policy": policies["baseline"],
        "playlist_placeholder_count": placeholders,
        "active_playlist_contract": playlist_contracts["baseline"],
        "snapshot": snapshot_relative,
        "snapshot_tree_sha256": snapshot_hash,
        "snapshot_files": tree_file_inventory(snapshot),
    }
    atomic_json(output, manifest)
    print(json.dumps({"ok": True, "output": str(output), "tree_sha256": snapshot_hash}))
    return 0


def seed_path_containment_self_test() -> None:
    def filesystem_snapshot(root: Path) -> list[tuple[str, str, bytes | None]]:
        result: list[tuple[str, str, bytes | None]] = []
        for path in [root, *sorted(root.rglob("*"))]:
            relative = "." if path == root else path.relative_to(root).as_posix()
            metadata = path.lstat()
            if stat.S_ISDIR(metadata.st_mode):
                result.append((relative, "directory", None))
            elif stat.S_ISREG(metadata.st_mode):
                result.append((relative, "regular", path.read_bytes()))
            elif stat.S_ISLNK(metadata.st_mode):
                result.append(
                    (
                        relative,
                        "symlink",
                        os.readlink(path).encode("utf-8", errors="surrogateescape"),
                    )
                )
            else:
                result.append((relative, f"mode:{metadata.st_mode}", None))
        return result

    def rejected_seed_contract(
        baseline: Path, candidate: Path, evidence_root: Path
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "seed-contract",
                "--scenarios",
                str(evidence_root / "must-not-be-read.json"),
                "--scenario",
                "playback_fresh_default",
                "--baseline-root",
                str(baseline),
                "--candidate-root",
                str(candidate),
                "--snapshot",
                str(evidence_root / "seed-template"),
                "--output",
                str(evidence_root / "seed-contract.json"),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

    with tempfile.TemporaryDirectory(
        prefix="ytt-perf-seed-containment-self-test-"
    ) as temporary:
        base = Path(temporary)

        baseline = base / "output-under-baseline"
        candidate = base / "output-under-baseline-candidate"
        baseline.mkdir()
        candidate.mkdir()
        (baseline / "seed-sentinel.bin").write_bytes(b"baseline-seed-must-not-change")
        (candidate / "seed-sentinel.bin").write_bytes(b"candidate-seed-must-not-change")
        evidence = baseline / "nested-evidence"
        before_baseline = filesystem_snapshot(baseline)
        before_candidate = filesystem_snapshot(candidate)
        result = rejected_seed_contract(baseline, candidate, evidence)
        if result.returncode == 0 or "must not equal, contain, or be contained by" not in result.stderr:
            raise AssertionError(
                "seed contract did not fail fast when output was under the baseline seed: "
                f"{result.stderr}"
            )
        if evidence.exists():
            raise AssertionError("rejected seed contract created output under its baseline seed")
        if (
            filesystem_snapshot(baseline) != before_baseline
            or filesystem_snapshot(candidate) != before_candidate
        ):
            raise AssertionError("rejected output-under-seed contract mutated a seed tree")

        outer_evidence = base / "baseline-under-output"
        nested_baseline = outer_evidence / "baseline-seed"
        second_candidate = base / "baseline-under-output-candidate"
        nested_baseline.mkdir(parents=True)
        second_candidate.mkdir()
        (nested_baseline / "seed-sentinel.bin").write_bytes(b"nested-baseline-must-not-change")
        (second_candidate / "seed-sentinel.bin").write_bytes(b"candidate-must-not-change")
        before_evidence = filesystem_snapshot(outer_evidence)
        before_second_candidate = filesystem_snapshot(second_candidate)
        result = rejected_seed_contract(
            nested_baseline, second_candidate, outer_evidence
        )
        if result.returncode == 0 or "must not equal, contain, or be contained by" not in result.stderr:
            raise AssertionError(
                "seed contract did not fail fast when the baseline seed was under output: "
                f"{result.stderr}"
            )
        if (
            (outer_evidence / "seed-contract.json").exists()
            or (outer_evidence / "seed-template").exists()
        ):
            raise AssertionError("rejected baseline-under-output contract created an output")
        if (
            filesystem_snapshot(outer_evidence) != before_evidence
            or filesystem_snapshot(second_candidate) != before_second_candidate
        ):
            raise AssertionError("rejected seed-under-output contract mutated a seed tree")

        valid_baseline = base / "valid-baseline"
        valid_candidate = base / "valid-candidate"
        for root in (valid_baseline, valid_candidate):
            for store in ("config", "data", "cache"):
                (root / "stores" / store).mkdir(parents=True)
            atomic_json(root / "stores" / "config" / "config.json", {})
            atomic_json(
                root / "stores" / "cache" / "session.json",
                {
                    "schema_version": 2,
                    "app_version": project_package_version(),
                    "last_mode": "normal",
                    "normal_queue": {
                        "songs": [{"local_path": "{{TUI_PERF_PLAYLIST}}"}],
                        "order": [0],
                        "cursor": 0,
                    },
                    "radio_queue": None,
                    "local_queue": None,
                },
            )
        valid_evidence = base / "valid-evidence"
        valid = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "seed-contract",
                "--scenarios",
                str(DEFAULT_SCENARIOS),
                "--scenario",
                "playback_fresh_default",
                "--baseline-root",
                str(valid_baseline),
                "--candidate-root",
                str(valid_candidate),
                "--snapshot",
                str(valid_evidence / "seed-template"),
                "--output",
                str(valid_evidence / "seed-contract.json"),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if valid.returncode != 0:
            raise AssertionError(f"disjoint seed contract failed: {valid.stderr}")
        if not (valid_evidence / "seed-contract.json").is_file() or not (
            valid_evidence / "seed-template"
        ).is_dir():
            raise AssertionError("disjoint seed contract did not create its expected siblings")


def validate_seed_contract_manifest(
    path: Path,
    evidence_root: Path,
    scenario: dict[str, Any],
    scenario_hash: str,
) -> tuple[dict[str, Any], Path]:
    evidence_root = evidence_root.resolve()
    manifest = load_json_object(path)
    require_artifact_value(path, "schema", manifest.get("schema"), SEED_CONTRACT_SCHEMA)
    require_artifact_value(path, "scenario", manifest.get("scenario"), scenario["id"])
    require_artifact_value(path, "scenario SHA-256", manifest.get("scenario_sha256"), scenario_hash)
    require_artifact_value(path, "seed contract", manifest.get("contract"), scenario["seed_contract"])
    source_hashes = manifest.get("source_tree_sha256")
    snapshot_digest = manifest.get("snapshot_tree_sha256")
    if (
        not isinstance(source_hashes, dict)
        or set(source_hashes) != {"baseline", "candidate"}
        or not isinstance(snapshot_digest, str)
        or any(value != snapshot_digest for value in source_hashes.values())
    ):
        raise ValueError(f"{path}: source seed trees are not identical to the snapshot")
    expected = scenario["seed_contract"].get("expected_cache_policy")
    if expected is not None:
        require_artifact_value(path, "cache policy", manifest.get("cache_policy"), expected)
    snapshot = (evidence_root / str(manifest.get("snapshot", ""))).resolve()
    try:
        snapshot.relative_to(evidence_root)
    except ValueError as error:
        raise ValueError(f"{path}: seed snapshot escapes evidence root") from error
    if not snapshot.is_dir():
        raise ValueError(f"{path}: seed snapshot does not exist: {snapshot}")
    reject_seed_symlinks(snapshot)
    require_artifact_value(
        path, "snapshot tree SHA-256", sha256_tree(snapshot), snapshot_digest
    )
    require_artifact_value(
        path, "snapshot cache policy", seed_cache_policy(snapshot), manifest.get("cache_policy")
    )
    snapshot_playlist_contract = validate_active_session_playlist(
        snapshot, "{{TUI_PERF_PLAYLIST}}"
    )
    placeholder_count = snapshot_playlist_contract["total_json_occurrences"]
    require_artifact_value(
        path,
        "playlist placeholder counts",
        manifest.get("playlist_placeholder_count"),
        {"baseline": placeholder_count, "candidate": placeholder_count},
    )
    require_artifact_value(
        path,
        "active playlist contract",
        manifest.get("active_playlist_contract"),
        snapshot_playlist_contract,
    )
    require_artifact_value(path, "snapshot file inventory", tree_file_inventory(snapshot), manifest.get("snapshot_files"))
    return manifest, snapshot


def overlay_tree_identity(
    base: Path, overlay: Path, changed: list[str]
) -> tuple[str, list[dict[str, Any]]]:
    reject_seed_symlinks(base)
    reject_seed_symlinks(overlay)
    base, base_files = regular_tree_files(base)
    overlay, overlay_files = regular_tree_files(overlay)
    overlay_paths = sorted(
        path.relative_to(overlay).as_posix()
        for path in overlay_files
    )
    if overlay_paths != sorted(changed):
        raise ValueError(
            f"materialized input snapshot contains {overlay_paths}, expected {sorted(changed)}"
        )
    selected = {
        path.relative_to(base).as_posix(): (path, base)
        for path in base_files
    }
    for relative in overlay_paths:
        selected[relative] = (overlay / relative, overlay)
    digest = hashlib.sha256()
    digest.update(TREE_DIGEST_DOMAIN)
    inventory = []
    for relative in sorted(selected):
        path, source_root = selected[relative]
        update_tree_digest(digest, source_root, path)
        inventory.append(
            {"path": relative, "bytes": path.stat().st_size, "sha256": sha256_file(path)}
        )
    return digest.hexdigest(), inventory


def tool_version(command: list[str]) -> dict[str, Any]:
    executable = shutil.which(command[0])
    if not executable:
        return {"available": False, "command": command}
    try:
        completed = subprocess.run(
            [executable, *command[1:]],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {
            "available": True,
            "path": executable,
            "command": command,
            "error": str(error),
        }
    output = (completed.stdout or completed.stderr).strip()
    return {
        "available": True,
        "path": executable,
        "command": command,
        "exit_code": completed.returncode,
        "version": output[:4096],
    }


def total_memory_bytes() -> int | None:
    try:
        pages = int(os.sysconf("SC_PHYS_PAGES"))
        page_size = int(os.sysconf("SC_PAGE_SIZE"))
        if pages > 0 and page_size > 0:
            return pages * page_size
    except (AttributeError, OSError, TypeError, ValueError):
        pass
    if os.name == "nt":
        try:
            import ctypes

            class MemoryStatus(ctypes.Structure):
                _fields_ = [
                    ("length", ctypes.c_ulong),
                    ("memory_load", ctypes.c_ulong),
                    ("total_physical", ctypes.c_ulonglong),
                    ("available_physical", ctypes.c_ulonglong),
                    ("total_page_file", ctypes.c_ulonglong),
                    ("available_page_file", ctypes.c_ulonglong),
                    ("total_virtual", ctypes.c_ulonglong),
                    ("available_virtual", ctypes.c_ulonglong),
                    ("available_extended_virtual", ctypes.c_ulonglong),
                ]

            status = MemoryStatus()
            status.length = ctypes.sizeof(status)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
                return int(status.total_physical)
        except (AttributeError, OSError, ValueError):
            pass
    return None


def cpu_model() -> str:
    identifier = os.environ.get("PROCESSOR_IDENTIFIER", "").strip()
    if identifier:
        return identifier
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.is_file():
        try:
            for line in cpuinfo.read_text(encoding="utf-8", errors="replace").splitlines():
                if line.lower().startswith(("model name", "hardware")) and ":" in line:
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    if platform.system() == "Darwin":
        version = tool_version(["sysctl", "-n", "machdep.cpu.brand_string"])
        if version.get("exit_code") == 0 and version.get("version"):
            return str(version["version"])
    return platform.processor() or platform.machine()


def command_manifest(args: argparse.Namespace) -> int:
    document, scenario_hash = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.scenario)
    render = scenario["id"] == "render_and_interaction"
    evidence_root = args.output.resolve().parent
    receipt_path = args.build_receipt.resolve()
    try:
        receipt_path.relative_to(evidence_root)
    except ValueError as error:
        raise ValueError("build receipt must stay inside the evidence root") from error
    receipt = load_json_object(receipt_path)
    sources = receipt.get("sources", {})
    if not isinstance(sources, dict):
        raise ValueError("build receipt sources are missing")
    baseline_root = Path(str(sources.get("baseline", {}).get("root", "")))
    candidate_root = Path(str(sources.get("candidate", {}).get("root", "")))
    validate_build_receipt(
        receipt, baseline_root, candidate_root, render, refresh=False
    )
    binaries = {
        label: {field: artifact[field] for field in ("path", "bytes", "sha256")}
        for label, artifact in receipt["artifacts"].items()
    }
    scenario_snapshot = evidence_root / "scenario.json"
    atomic_bytes(scenario_snapshot, args.scenarios.read_bytes())
    if sha256_file(scenario_snapshot) != scenario_hash:
        raise ValueError("scenario snapshot changed while being copied")
    tool_commands = {
        "python": [sys.executable, "--version"],
        "mpv_on_path": ["mpv", "--version"],
        "yt_dlp_on_path": ["yt-dlp", "--version"],
        "tmux": ["tmux", "-V"],
        "rustc": ["rustc", "--version"],
        "cargo": ["cargo", "--version"],
    }
    if os.name == "nt":
        tool_commands["powershell"] = [
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ]
    manifest = {
        "schema": "ytt.tui-perf.host.v1",
        "scenario": args.scenario,
        "scenario_sha256": scenario_hash,
        "scenario_file": {
            "path": scenario_snapshot.relative_to(evidence_root).as_posix(),
            "bytes": scenario_snapshot.stat().st_size,
            "sha256": scenario_hash,
        },
        "generated_unix_s": int(time.time()),
        "host": {
            **stable_host_identity(),
            "version": platform.version(),
            "cpu_model": cpu_model(),
            "logical_cpu_count": os.cpu_count(),
            "total_memory_bytes": total_memory_bytes(),
        },
        "tools": {name: tool_version(command) for name, command in tool_commands.items()},
        "binaries": binaries,
        "sources": sources,
        "build_receipt": {
            **identity_for_file(receipt_path),
            "path": receipt_path.relative_to(evidence_root).as_posix(),
        },
        "orchestrator": receipt["orchestrator"],
        "measurement_scope": document["sampling"],
        "limitations": measurement_limitations(render),
        "note": "actual mpv argv and executable are recorded in each sampler artifact",
    }
    atomic_json(args.output, manifest)
    print(json.dumps({"ok": True, "output": str(args.output), "scenario_sha256": scenario_hash}))
    return 0


def resolve_materialize_output_paths(
    args: argparse.Namespace, root: Path
) -> tuple[Path, Path]:
    manifest = args.manifest.resolve()
    input_snapshot = args.input_snapshot.resolve()
    outputs = (
        ("--manifest", args.manifest, manifest),
        ("--input-snapshot", args.input_snapshot, input_snapshot),
    )
    for label, supplied, resolved in outputs:
        if resolved_paths_overlap(resolved, root):
            raise ValueError(f"{label} must stay outside the mutable home")
        if path_entry_exists(supplied) or path_entry_exists(resolved):
            raise ValueError(f"{label} must name a new path")
    if manifest == input_snapshot:
        raise ValueError("--manifest and --input-snapshot must be distinct paths")
    if input_snapshot.parent != manifest.parent:
        raise ValueError("--input-snapshot must be directly beside --manifest")
    if not manifest.parent.is_dir():
        raise ValueError("materialize output parent must be an existing directory")
    return manifest, input_snapshot


def command_materialize(args: argparse.Namespace) -> int:
    if not args.root.is_dir():
        raise ValueError(f"seed root does not exist: {args.root}")
    root = args.root.resolve()
    home = args.home.resolve()
    if home != root:
        raise ValueError("--home must equal --root so the initial-state digest is unambiguous")
    reject_seed_symlinks(root)
    seed_tree_sha256 = sha256_tree(root)
    cache_policy = seed_cache_policy(root)
    seed_playlist_contract = validate_active_session_playlist(
        root, "{{TUI_PERF_PLAYLIST}}"
    )
    playlist = (root / args.playlist_relative).resolve()
    try:
        playlist.relative_to(root)
    except ValueError as error:
        raise ValueError("--playlist-relative must stay inside --root") from error
    manifest_path, input_snapshot = resolve_materialize_output_paths(args, root)

    parsed_url = urlsplit(args.fixture_url)
    try:
        fixture_ip = ipaddress.ip_address(parsed_url.hostname or "")
    except ValueError as error:
        raise ValueError("--fixture-url host must be a loopback IP literal") from error
    if parsed_url.scheme != "http" or not fixture_ip.is_loopback:
        raise ValueError("--fixture-url must be an HTTP loopback URL")

    replacements = {
        "{{TUI_PERF_FIXTURE_URL}}": args.fixture_url,
        "{{TUI_PERF_HOME}}": str(home),
        "{{TUI_PERF_PLAYLIST}}": str(playlist),
    }
    seed_files: list[tuple[Path, str]] = []
    playlist_references = 0
    for path in sorted(item for item in args.root.rglob("*") if item.is_file()):
        if path.resolve() == playlist:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if path.suffix.lower() == ".json" and "{{TUI_PERF_FIXTURE_URL}}" in text:
            raise ValueError(
                f"{path}: JSON must reference {{{{TUI_PERF_PLAYLIST}}}} as Song.local_path; "
                "a direct loopback URL is rejected by the playback guard"
            )
        if path.suffix.lower() == ".json":
            playlist_references += text.count("{{TUI_PERF_PLAYLIST}}")
        seed_files.append((path, text))
    require_artifact_value(
        root,
        "playlist marker references",
        playlist_references,
        seed_playlist_contract["total_json_occurrences"],
    )

    atomic_text(
        playlist,
        f"#EXTM3U\n#EXTINF:-1,ytt deterministic performance fixture\n{args.fixture_url}\n",
    )
    changed_paths = [playlist]
    for path, text in seed_files:
        if not any(marker in text for marker in replacements):
            continue
        for marker, replacement in replacements.items():
            encoded = json.dumps(replacement)[1:-1] if path.suffix.lower() == ".json" else replacement
            text = text.replace(marker, encoded)
        if path.suffix.lower() == ".json":
            try:
                json.loads(text, object_pairs_hook=reject_duplicate_json_keys)
            except (json.JSONDecodeError, DuplicateJsonKeyError) as error:
                raise ValueError(f"materialized JSON is invalid at {path}: {error}") from error
        atomic_text(path, text)
        changed_paths.append(path)
    changed_paths.append(materialize_single_song_active_session(root, str(playlist)))
    changed = sorted(
        {
            path.resolve().relative_to(root).as_posix()
            for path in changed_paths
        }
    )
    materialized_playlist_contract = validate_materialized_active_session_playlist(
        root, str(playlist)
    )
    input_snapshot.mkdir(parents=True)
    for relative in changed:
        source = root / relative
        destination = input_snapshot / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    materialized_tree_sha256 = sha256_tree(root)
    playlist_relative = playlist.relative_to(root).as_posix()
    expected_playlist = (
        f"#EXTM3U\n#EXTINF:-1,ytt deterministic performance fixture\n{args.fixture_url}\n"
    )
    if playlist.read_text(encoding="utf-8") != expected_playlist:
        raise ValueError("materialized playlist does not contain the exact fixture URL")
    manifest = {
        "schema": "ytt.tui-perf.materialize.v1",
        "changed": changed,
        "fixture_url": args.fixture_url,
        "fixture_host": str(fixture_ip),
        "playlist": playlist_relative,
        "playlist_sha256": sha256_file(playlist),
        "playback_target_mode": "local_m3u_indirection",
        "external_dns_required": False,
        "playlist_references": playlist_references,
        "seed_active_playlist_contract": seed_playlist_contract,
        "materialized_active_playlist_contract": materialized_playlist_contract,
        "seed_label": args.seed_label,
        "seed_tree_sha256": seed_tree_sha256,
        "seed_cache_policy": cache_policy,
        "materialized_tree_sha256": materialized_tree_sha256,
        "materialized_files": tree_file_inventory(root),
        "input_snapshot": input_snapshot.name,
        "input_snapshot_files": tree_file_inventory(input_snapshot),
        "materializer_sha256": sha256_file(Path(__file__)),
    }
    atomic_json(manifest_path, manifest)
    print(json.dumps(manifest, sort_keys=True))
    return 0


def launch_policy_projection(document: dict[str, Any]) -> dict[str, Any]:
    """Return the config with only launch-policy-owned leaves removed.

    The projection binds every unrelated value without serializing those values into the
    manifest.  Empty policy-only containers are ignored so adding a previously absent
    ``tools`` or ``scrobble`` object is not mistaken for a secret-bearing config change.
    """

    omitted = object()

    def project(value: Any, path: tuple[str, ...]) -> Any:
        if path in LAUNCH_POLICY_FIELDS:
            return omitted
        if isinstance(value, dict):
            result = {}
            for key, item in value.items():
                child = project(item, (*path, key))
                if child is not omitted:
                    result[key] = child
            owns_descendant = bool(path) and any(
                field[: len(path)] == path for field in LAUNCH_POLICY_FIELDS
            )
            if not result and owns_descendant:
                return omitted
            return result
        if isinstance(value, list):
            return [project(item, (*path, str(index))) for index, item in enumerate(value)]
        return value

    projected = project(document, ())
    if not isinstance(projected, dict):
        raise AssertionError("launch-policy projection must remain an object")
    return projected


def json_value_sha256(value: Any) -> str:
    encoded = json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def inline_cookie_has_session(value: Any) -> bool:
    if value is None:
        return False
    if not isinstance(value, str):
        raise ValueError("launch config cookie must be a string or null")
    for pair in value.split(";"):
        name, separator, _cookie_value = pair.strip().partition("=")
        if separator and name.strip() in {"SAPISID", "__Secure-3PAPISID"}:
            return True
    return False


def netscape_cookie_file_has_session(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ValueError("cannot inspect a possible default cookies file") from error
    if path.is_symlink() or not path.is_file() or metadata.st_size > 4 * 1024 * 1024:
        raise ValueError("possible default cookies file is not a safe bounded regular file")
    try:
        content = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise ValueError("cannot safely decode a possible default cookies file") from error
    for raw in content.splitlines():
        line = raw.removeprefix("#HttpOnly_")
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < 7:
            continue
        domain = fields[0].strip().lstrip(".")
        if domain != "youtube.com" and not domain.endswith(".youtube.com"):
            continue
        if fields[5].strip() in {"SAPISID", "__Secure-3PAPISID"}:
            return True
    return False


def launch_api_credential_proof(document: dict[str, Any], root: Path) -> dict[str, Any]:
    """Fail closed when startup cookie authentication could contact YouTube Music.

    ``api::spawn`` initializes a configured browser token immediately; ytmapi obtains its
    client version with an external GET.  The performance seed must therefore prove that no
    session-bearing inline/default cookie can reach that path.  Values are never returned or
    included in an error.
    """

    if inline_cookie_has_session(document.get("cookie")):
        raise ValueError("launch policy rejects a credential-bearing inline API cookie")
    explicit = document.get("cookies_file")
    if explicit is not None:
        if not isinstance(explicit, str):
            raise ValueError("launch config cookies_file must be a string or null")
        # Even an empty PathBuf is Some(path) in Config.  Reject every explicit value so a
        # relative path cannot resolve against an orchestrator-dependent working directory.
        raise ValueError("launch policy rejects an explicit API cookies_file path")
    user_dirs = root / ".config" / "user-dirs.dirs"
    if user_dirs.exists():
        raise ValueError("launch policy rejects an XDG user-dirs override")
    credential_files = 0
    for candidate in sorted(
        path for path in root.rglob("*") if path.name.lower() == "cookies.txt"
    ):
        credential_files += int(netscape_cookie_file_has_session(candidate))
    if credential_files:
        raise ValueError("launch policy rejects a credential-bearing default API cookie file")
    return {
        "inline_session_cookie_present": False,
        "explicit_cookies_file_configured": False,
        "credential_bearing_default_cookie_files": 0,
        "user_dirs_override_present": False,
    }


def apply_launch_policy(document: dict[str, Any]) -> None:
    tools_config = document.setdefault("tools", {})
    if not isinstance(tools_config, dict):
        raise ValueError("launch config tools must be an object")
    scrobble = document.setdefault("scrobble", {})
    if not isinstance(scrobble, dict):
        raise ValueError("launch config scrobble must be an object")
    lastfm = scrobble.setdefault("lastfm", {})
    if not isinstance(lastfm, dict):
        raise ValueError("launch config scrobble.lastfm must be an object")
    listenbrainz = scrobble.setdefault("listenbrainz", {})
    if not isinstance(listenbrainz, dict):
        raise ValueError("launch config scrobble.listenbrainz must be an object")
    tools_config["ytdlp_managed"] = False
    document["update_check_enabled"] = False
    document["media_controls"] = False
    document["autoplay_on_start"] = False
    document["autoplay_streaming"] = False
    document["album_art"] = False
    document["romanized_titles"] = False
    document["ai_enabled"] = False
    lastfm["enabled"] = False
    listenbrainz["enabled"] = False
    scrobble["local_files"] = False


def validate_effective_launch_config(path: Path, document: dict[str, Any]) -> None:
    expected = {
        ("tools", "ytdlp_managed"): False,
        ("update_check_enabled",): False,
        ("media_controls",): False,
        ("autoplay_on_start",): False,
        ("autoplay_streaming",): False,
        ("album_art",): False,
        ("romanized_titles",): False,
        ("ai_enabled",): False,
        ("scrobble", "lastfm", "enabled"): False,
        ("scrobble", "listenbrainz", "enabled"): False,
        ("scrobble", "local_files"): False,
    }
    for fields, value in expected.items():
        current: Any = document
        for field in fields:
            if not isinstance(current, dict) or field not in current:
                raise ValueError(f"{path}: launch policy field {'.'.join(fields)} is missing")
            current = current[field]
        require_artifact_value(path, f"launch policy {'.'.join(fields)}", current, value)


def resolve_launch_policy_output_paths(
    args: argparse.Namespace, root: Path
) -> tuple[Path, Path]:
    output = args.output.resolve()
    snapshot = output.parent / "launch-policy-inputs"
    for label, supplied, resolved in (
        ("launch-policy output", args.output, output),
        ("launch-policy snapshot", snapshot, snapshot),
    ):
        if resolved_paths_overlap(resolved, root):
            raise ValueError(f"{label} must stay outside the mutable home")
        if path_entry_exists(supplied) or path_entry_exists(resolved):
            raise ValueError(f"{label} must name a new path")
    if output == snapshot:
        raise ValueError("launch-policy output and snapshot must be distinct paths")
    if not output.parent.is_dir():
        raise ValueError("launch-policy output parent must be an existing directory")
    return output, snapshot


def command_launch_policy(args: argparse.Namespace) -> int:
    root = args.root.resolve()
    if not root.is_dir():
        raise ValueError(f"launch-policy root does not exist: {root}")
    output, snapshot = resolve_launch_policy_output_paths(args, root)
    config = root / "stores" / "config" / "config.json"
    config.parent.mkdir(parents=True, exist_ok=True)
    before = identity_for_file(config) if config.is_file() else None
    if config.is_file():
        try:
            document = json.loads(
                config.read_text(encoding="utf-8"),
                object_pairs_hook=reject_duplicate_json_keys,
            )
        except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
            raise ValueError(f"cannot freeze invalid launch config {config}: {error}") from error
        if not isinstance(document, dict):
            raise ValueError(f"{config}: launch config must be an object")
    else:
        document = {}
    credential_proof = launch_api_credential_proof(document, root)
    preserved_projection = launch_policy_projection(document)
    apply_launch_policy(document)
    if launch_policy_projection(document) != preserved_projection:
        raise AssertionError("launch policy changed a non-policy config field")
    validate_effective_launch_config(config, document)
    atomic_json(config, document)
    snapshot.mkdir(parents=True)
    snapshot_policy = snapshot / "effective-policy.json"
    atomic_json(
        snapshot_policy,
        {
            "schema": "ytt.tui-perf.launch-policy-input.v1",
            "effective": LAUNCH_POLICY_EFFECTIVE,
            "api_credential_proof": credential_proof,
            "child_environment_policy": CHILD_ENVIRONMENT_POLICY,
            "policy_fields": [".".join(fields) for fields in LAUNCH_POLICY_FIELDS],
        },
    )
    manifest = {
        "schema": "ytt.tui-perf.launch-policy.v1",
        "root": str(root),
        "config": "stores/config/config.json",
        "config_before": before,
        "config_after": identity_for_file(config),
        "snapshot": snapshot.name,
        "snapshot_policy": snapshot_policy.name,
        "snapshot_files": tree_file_inventory(snapshot),
        "policy_fields": [".".join(fields) for fields in LAUNCH_POLICY_FIELDS],
        "preserved_config_projection_sha256": json_value_sha256(preserved_projection),
        "api_credential_proof": credential_proof,
        "child_environment_policy": CHILD_ENVIRONMENT_POLICY,
        "effective": LAUNCH_POLICY_EFFECTIVE,
    }
    atomic_json(output, manifest)
    print(json.dumps({"ok": True, "output": str(output), "snapshot": str(snapshot)}))
    return 0


def unix_process_observation(pid: int, *, hash_executable: bool = True) -> dict[str, Any] | None:
    if pid <= 0:
        return None
    if sys.platform.startswith("linux"):
        proc = Path("/proc") / str(pid)
        try:
            raw_stat = (proc / "stat").read_text(encoding="utf-8")
            closing = raw_stat.rfind(")")
            fields = raw_stat[closing + 2 :].split()
            start_ticks = int(fields[19])
            boot_time = next(
                int(line.split()[1])
                for line in Path("/proc/stat").read_text(encoding="utf-8").splitlines()
                if line.startswith("btime ")
            )
            ticks = int(os.sysconf("SC_CLK_TCK"))
            executable = (proc / "exe").resolve(strict=True)
            command = [
                part.decode("utf-8", errors="surrogateescape")
                for part in (proc / "cmdline").read_bytes().split(b"\0")
                if part
            ]
        except (FileNotFoundError, ProcessLookupError):
            return None
        except (OSError, ValueError, StopIteration) as error:
            raise ValueError(f"cannot inspect Linux PID {pid}: {error}") from error
        return {
            "pid": pid,
            "parent_pid": int(fields[1]),
            "process_group_id": int(fields[2]),
            "start_time_unix_s": boot_time + start_ticks // ticks,
            "executable": str(executable),
            "executable_bytes": executable.stat().st_size,
            "executable_sha256": sha256_file(executable) if hash_executable else None,
            "command": command,
        }
    if sys.platform == "darwin":
        try:
            import ctypes

            buffer = ctypes.create_string_buffer(4096)
            libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
            length = libproc.proc_pidpath(pid, buffer, len(buffer))
            if length <= 0:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    return None
                environment = controlled_build_environment()
                environment["LC_ALL"] = "C"
                state = subprocess.run(
                    ["/bin/ps", "-p", str(pid), "-o", "state="],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    env=environment,
                    check=False,
                )
                if state.returncode == 0 and state.stdout.strip().startswith("Z"):
                    return None
                raise ValueError(f"proc_pidpath failed for PID {pid}")
            executable = Path(os.fsdecode(buffer.value)).resolve(strict=True)
            libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
            mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2, pid
            size = ctypes.c_size_t()
            if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0 or size.value == 0:
                raise ValueError(f"KERN_PROCARGS2 size query failed for PID {pid}")
            argv_buffer = ctypes.create_string_buffer(size.value)
            if libc.sysctl(
                mib, 3, argv_buffer, ctypes.byref(size), None, 0
            ) != 0:
                raise ValueError(f"KERN_PROCARGS2 read failed for PID {pid}")
            raw_argv = argv_buffer.raw[: size.value]
            if len(raw_argv) < struct.calcsize("i"):
                raise ValueError(f"KERN_PROCARGS2 result is truncated for PID {pid}")
            argc = struct.unpack_from("i", raw_argv)[0]
            cursor = struct.calcsize("i")
            executable_end = raw_argv.find(b"\0", cursor)
            if argc < 0 or executable_end < 0:
                raise ValueError(f"KERN_PROCARGS2 header is invalid for PID {pid}")
            cursor = executable_end + 1
            while cursor < len(raw_argv) and raw_argv[cursor] == 0:
                cursor += 1
            command: list[str] = []
            for _ in range(argc):
                end = raw_argv.find(b"\0", cursor)
                if end < 0:
                    raise ValueError(f"KERN_PROCARGS2 argv is truncated for PID {pid}")
                command.append(raw_argv[cursor:end].decode("utf-8", errors="surrogateescape"))
                cursor = end + 1
            environment = controlled_build_environment()
            environment["LC_ALL"] = "C"
            completed = subprocess.run(
                ["/bin/ps", "-p", str(pid), "-o", "lstart="],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
                check=False,
            )
            if completed.returncode != 0 or not completed.stdout.strip():
                return None
            line = completed.stdout.strip()
            match = re.fullmatch(
                r"(\w{3}\s+\w{3}\s+\d+\s+\d\d:\d\d:\d\d\s+\d{4})",
                line,
            )
            if not match:
                raise ValueError(f"cannot parse ps start/command for PID {pid}: {line!r}")
            start = int(time.mktime(time.strptime(match.group(1), "%a %b %d %H:%M:%S %Y")))
            relation = subprocess.run(
                ["/bin/ps", "-p", str(pid), "-o", "ppid=", "-o", "pgid="],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
                check=False,
            )
            relation_fields = relation.stdout.split()
            if relation.returncode != 0 or len(relation_fields) != 2:
                return None
            parent_pid, process_group_id = map(int, relation_fields)
        except ProcessLookupError:
            return None
        except (OSError, ValueError) as error:
            raise ValueError(f"cannot inspect macOS PID {pid}: {error}") from error
        return {
            "pid": pid,
            "parent_pid": parent_pid,
            "process_group_id": process_group_id,
            "start_time_unix_s": start,
            "executable": str(executable),
            "executable_bytes": executable.stat().st_size,
            "executable_sha256": sha256_file(executable) if hash_executable else None,
            "command": command,
        }
    raise ValueError("exact cleanup is supported only on Linux and macOS")


def native_process_start_token(pid: int) -> str | None:
    """Return an OS-native token that changes when a numeric PID is reused."""
    if pid <= 0:
        return None
    if sys.platform.startswith("linux"):
        try:
            raw_stat = (Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
            closing = raw_stat.rfind(")")
            if closing < 0:
                raise ValueError("missing comm terminator")
            fields = raw_stat[closing + 2 :].split()
            start_ticks = int(fields[19])
            boot_fingerprint = host_identifier_fingerprint(
                "boot_id", stable_boot_id("Linux")
            )
        except (FileNotFoundError, ProcessLookupError):
            return None
        except (OSError, ValueError, IndexError) as error:
            raise ValueError(
                f"cannot read native Linux start token for PID {pid}: {error}"
            ) from error
        return f"linux-proc-start:{boot_fingerprint}:{start_ticks}"
    if sys.platform == "darwin":
        try:
            import ctypes

            class ProcBsdInfo(ctypes.Structure):
                _fields_ = [
                    ("pbi_flags", ctypes.c_uint32),
                    ("pbi_status", ctypes.c_uint32),
                    ("pbi_xstatus", ctypes.c_uint32),
                    ("pbi_pid", ctypes.c_uint32),
                    ("pbi_ppid", ctypes.c_uint32),
                    ("pbi_uid", ctypes.c_uint32),
                    ("pbi_gid", ctypes.c_uint32),
                    ("pbi_ruid", ctypes.c_uint32),
                    ("pbi_rgid", ctypes.c_uint32),
                    ("pbi_svuid", ctypes.c_uint32),
                    ("pbi_svgid", ctypes.c_uint32),
                    ("rfu_1", ctypes.c_uint32),
                    ("pbi_comm", ctypes.c_char * 16),
                    ("pbi_name", ctypes.c_char * 32),
                    ("pbi_nfiles", ctypes.c_uint32),
                    ("pbi_pgid", ctypes.c_uint32),
                    ("pbi_pjobc", ctypes.c_uint32),
                    ("e_tdev", ctypes.c_uint32),
                    ("e_tpgid", ctypes.c_uint32),
                    ("pbi_nice", ctypes.c_int32),
                    ("pbi_start_tvsec", ctypes.c_uint64),
                    ("pbi_start_tvusec", ctypes.c_uint64),
                ]

            info = ProcBsdInfo()
            libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
            libproc.proc_pidinfo.argtypes = [
                ctypes.c_int,
                ctypes.c_int,
                ctypes.c_uint64,
                ctypes.c_void_p,
                ctypes.c_int,
            ]
            libproc.proc_pidinfo.restype = ctypes.c_int
            read_size = libproc.proc_pidinfo(
                pid,
                3,  # PROC_PIDTBSDINFO
                0,
                ctypes.byref(info),
                ctypes.sizeof(info),
            )
            if read_size != ctypes.sizeof(info):
                state = unix_process_state(pid)
                if state is None or state.startswith("Z"):
                    return None
                error_number = ctypes.get_errno()
                raise ValueError(
                    "proc_pidinfo returned "
                    f"{read_size}/{ctypes.sizeof(info)} bytes (errno {error_number})"
                )
            if info.pbi_pid != pid or info.pbi_start_tvsec <= 0:
                raise ValueError("proc_pidinfo returned an invalid process identity")
        except ProcessLookupError:
            return None
        except (OSError, ValueError) as error:
            raise ValueError(
                f"cannot read native macOS start token for PID {pid}: {error}"
            ) from error
        return f"darwin-proc-start:{info.pbi_start_tvsec}:{info.pbi_start_tvusec}"
    raise ValueError("native process start tokens are supported only on Linux and macOS")


def fixture_server_process_observation(pid: int) -> dict[str, Any] | None:
    token_before = native_process_start_token(pid)
    if token_before is None:
        return None
    observation = unix_process_observation(pid, hash_executable=True)
    token_after = native_process_start_token(pid)
    if observation is None or token_after is None or token_before != token_after:
        return None
    command = observation.get("command")
    if not isinstance(command, list) or not command or not all(
        isinstance(item, str) for item in command
    ):
        raise ValueError(f"fixture server PID {pid} has no exact argv identity")
    return {
        field: observation[field]
        for field in (
            "pid",
            "start_time_unix_s",
            "executable",
            "executable_bytes",
            "executable_sha256",
            "command",
        )
    } | {"native_start_token": token_before}


def fixture_server_identity_matches(
    identity: dict[str, Any], observation: dict[str, Any] | None
) -> bool:
    if observation is None:
        return False
    return all(
        observation.get(field) == identity.get(field)
        for field in (
            "pid",
            "start_time_unix_s",
            "native_start_token",
            "executable",
            "executable_bytes",
            "executable_sha256",
            "command",
        )
    )


def validated_live_identity(
    document: dict[str, Any], path: Path
) -> tuple[
    dict[str, Any],
    dict[str, Any] | None,
    list[dict[str, Any]],
    list[dict[str, Any]],
]:
    require_artifact_value(path, "schema", document.get("schema"), "ytt.tui-perf.live-identity.v1")
    require_artifact_value(
        path, "cleanup scope", document.get("cleanup_scope"), CLEANUP_SCOPE
    )
    if not isinstance(document.get("run_id"), str) or not document["run_id"]:
        raise ValueError(f"{path}: live identity run_id is missing")
    state = document.get("state")
    if state not in {"startup", "owner_starting", "running", "cleanup_requested", "cleaned"}:
        raise ValueError(
            f"{path}: invalid live identity lifecycle state {state!r}"
        )
    producer = document.get("producer")
    owner = document.get("owner")
    partial_owner = document.get("partial_owner")
    mpv = document.get("mpv")
    descendants = document.get("descendants")
    if (
        not isinstance(producer, dict)
        or (owner is not None and not isinstance(owner, dict))
        or (partial_owner is not None and not isinstance(partial_owner, dict))
        or not isinstance(mpv, list)
        or not all(isinstance(item, dict) for item in mpv)
        or not isinstance(descendants, list)
        or not all(isinstance(item, dict) for item in descendants)
    ):
        raise ValueError(f"{path}: live producer/owner/descendant identity is malformed")
    if state == "running" and not isinstance(owner, dict):
        raise ValueError(f"{path}: {state} live identity requires a complete owner")
    if state == "owner_starting" and not isinstance(partial_owner, dict):
        raise ValueError(f"{path}: owner_starting identity requires partial_owner")

    def validate_core(label: str, identity: dict[str, Any]) -> None:
        for field in ("pid", "start_time_unix_s", "executable_bytes"):
            value = non_negative_integer(identity.get(field), f"{label} {field}", path)
            if value == 0:
                raise ValueError(f"{path}: {label} {field} must be positive")
        executable = identity.get("executable")
        digest = identity.get("executable_sha256")
        if (
            not isinstance(executable, str)
            or not executable
            or not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", digest) is None
        ):
            raise ValueError(f"{path}: {label} executable identity is invalid")

    validate_core("producer", producer)
    producer_group = non_negative_integer(
        producer.get("process_group_id"), "producer process_group_id", path
    )
    if producer_group == 0:
        raise ValueError(f"{path}: producer process_group_id must be positive")
    if isinstance(owner, dict):
        validate_core("owner", owner)
        owner_group = non_negative_integer(
            owner.get("process_group_id"), "owner process_group_id", path
        )
        if owner_group == 0:
            raise ValueError(f"{path}: owner process_group_id must be positive")
        if owner_group != owner["pid"]:
            raise ValueError(
                f"{path}: owner must lead its dedicated process group "
                f"(PID {owner['pid']}, PGID {owner_group})"
            )
        if owner_group == producer_group:
            raise ValueError(f"{path}: owner process group must differ from producer group")
        if producer["pid"] == owner["pid"]:
            raise ValueError(f"{path}: producer and owner PID must differ")
    if isinstance(partial_owner, dict):
        for field in ("pid", "start_time_unix_s", "process_group_id"):
            value = non_negative_integer(
                partial_owner.get(field), f"partial_owner {field}", path
            )
            if value == 0:
                raise ValueError(f"{path}: partial_owner {field} must be positive")
        if partial_owner["pid"] == producer["pid"]:
            raise ValueError(f"{path}: producer and partial owner PID must differ")
        if partial_owner["process_group_id"] != partial_owner["pid"]:
            raise ValueError(
                f"{path}: partial owner must lead its dedicated process group "
                f"(PID {partial_owner['pid']}, PGID {partial_owner['process_group_id']})"
            )
        if partial_owner["process_group_id"] == producer_group:
            raise ValueError(
                f"{path}: partial owner process group must differ from producer group"
            )
        if isinstance(owner, dict):
            for field in ("pid", "start_time_unix_s", "process_group_id"):
                require_artifact_value(
                    path,
                    f"partial/full owner {field}",
                    partial_owner[field],
                    owner[field],
                )
    descendant_by_pid: dict[int, dict[str, Any]] = {}
    for index, identity in enumerate(descendants):
        label = f"descendants[{index}]"
        validate_core(label, identity)
        pid = identity["pid"]
        reserved_pids = {producer["pid"]}
        if isinstance(owner, dict):
            reserved_pids.add(owner["pid"])
        if pid in reserved_pids or pid in descendant_by_pid:
            raise ValueError(f"{path}: duplicate live identity PID {pid}")
        role = identity.get("role")
        command = identity.get("command")
        if role not in {"mpv", "other"}:
            raise ValueError(f"{path}: {label} role must be mpv or other")
        if not isinstance(command, list) or not command or not all(
            isinstance(item, str) for item in command
        ):
            raise ValueError(f"{path}: {label} exact command is invalid")
        descendant_by_pid[pid] = identity
    seen_mpv: set[int] = set()
    for index, identity in enumerate(mpv):
        label = f"mpv[{index}]"
        validate_core(label, identity)
        pid = identity["pid"]
        if pid in seen_mpv:
            raise ValueError(f"{path}: duplicate mpv identity PID {pid}")
        seen_mpv.add(pid)
        argv = identity.get("input_ipc_server_argv")
        if not isinstance(argv, list) or not argv or not all(isinstance(item, str) for item in argv):
            raise ValueError(f"{path}: {label} IPC argv identity is invalid")
        descendant = descendant_by_pid.get(pid)
        if descendant is None or descendant.get("role") != "mpv":
            raise ValueError(f"{path}: {label} is not present in the recursive descendant inventory")
        for field in (
            "pid",
            "start_time_unix_s",
            "executable",
            "executable_bytes",
            "executable_sha256",
        ):
            require_artifact_value(path, f"{label} {field}", identity.get(field), descendant.get(field))
        observed_argv, _endpoint = mpv_ipc_identity(descendant["command"])
        require_artifact_value(path, f"{label} IPC argv", argv, observed_argv)
    return producer, owner, mpv, descendants


def live_identity_matches(
    identity: dict[str, Any],
    observation: dict[str, Any] | None,
    *,
    mpv: bool = False,
    exact_command: bool = False,
    verify_hash: bool = True,
) -> bool:
    if observation is None:
        return False
    fields = ["pid", "start_time_unix_s", "executable", "executable_bytes"]
    if verify_hash:
        fields.append("executable_sha256")
    for field in fields:
        if observation.get(field) != identity.get(field):
            return False
    if exact_command and observation.get("command") != identity.get("command"):
        return False
    if mpv:
        argv, _endpoint = mpv_ipc_identity(observation.get("command", []))
        return argv == identity.get("input_ipc_server_argv")
    return True


def unix_process_relations() -> dict[int, tuple[int, int]]:
    if sys.platform.startswith("linux"):
        relations: dict[int, tuple[int, int]] = {}
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                raw_stat = (entry / "stat").read_text(encoding="utf-8")
                closing = raw_stat.rfind(")")
                fields = raw_stat[closing + 2 :].split()
                relations[int(entry.name)] = (int(fields[1]), int(fields[2]))
            except (FileNotFoundError, ProcessLookupError, OSError, ValueError, IndexError):
                continue
        return relations
    if sys.platform == "darwin":
        environment = controlled_build_environment()
        environment["LC_ALL"] = "C"
        completed = subprocess.run(
            ["/bin/ps", "-axo", "pid=", "-o", "ppid=", "-o", "pgid="],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            check=False,
        )
        if completed.returncode != 0:
            raise ValueError(f"cannot inventory process relations: {completed.stderr.strip()}")
        relations = {}
        for line in completed.stdout.splitlines():
            fields = line.split()
            if len(fields) == 3:
                pid, parent, group = map(int, fields)
                relations[pid] = (parent, group)
        return relations
    raise ValueError("exact cleanup is supported only on Linux and macOS")


def unix_process_state(pid: int) -> str | None:
    if sys.platform.startswith("linux"):
        try:
            raw_stat = (Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
            closing = raw_stat.rfind(")")
            if closing < 0:
                raise ValueError("missing comm terminator")
            fields = raw_stat[closing + 2 :].split()
            return fields[0] if fields else None
        except (FileNotFoundError, ProcessLookupError):
            return None
        except (OSError, ValueError) as error:
            raise ValueError(f"cannot inspect Linux process state for PID {pid}: {error}") from error
    if sys.platform == "darwin":
        environment = controlled_build_environment()
        environment["LC_ALL"] = "C"
        completed = subprocess.run(
            ["/bin/ps", "-p", str(pid), "-o", "state="],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            check=False,
        )
        if completed.returncode != 0:
            return None
        state = completed.stdout.strip()
        return state or None
    raise ValueError("exact cleanup is supported only on Linux and macOS")


def wait_for_process_group_stopped(group: int, deadline: float) -> list[int]:
    last_states: dict[int, str | None] = {}
    while time.monotonic() < deadline:
        relations = unix_process_relations()
        members = sorted(
            pid for pid, (_parent, process_group) in relations.items() if process_group == group
        )
        last_states = {pid: unix_process_state(pid) for pid in members}
        if members and all(
            state is not None and state[:1] in {"T", "t"}
            for state in last_states.values()
        ):
            return members
        time.sleep(0.01)
    raise ValueError(
        f"dedicated owner process group {group} did not become fully stopped: {last_states}"
    )


def cleanup_descendant_identity(observation: dict[str, Any]) -> dict[str, Any]:
    command = observation.get("command")
    if not isinstance(command, list) or not command:
        raise ValueError(f"PID {observation.get('pid')} has no exact argv for cleanup")
    executable_name = Path(str(observation["executable"])).stem.lower()
    argv0 = Path(command[0]).stem.lower()
    ipc_argv, _endpoint = mpv_ipc_identity(command)
    role = (
        "mpv"
        if executable_name == "mpv" or argv0 == "mpv" or ipc_argv is not None
        else "other"
    )
    return {
        field: observation[field]
        for field in (
            "pid",
            "start_time_unix_s",
            "executable",
            "executable_bytes",
            "executable_sha256",
        )
    } | {"role": role, "command": command}


def mpv_identities_from_descendants(descendants: list[dict[str, Any]]) -> list[dict[str, Any]]:
    result = []
    for descendant in descendants:
        if descendant["role"] != "mpv":
            continue
        argv, _endpoint = mpv_ipc_identity(descendant["command"])
        if argv is None:
            raise ValueError(
                f"mpv PID {descendant['pid']} has no exact --input-ipc-server argv"
            )
        result.append(
            {
                field: descendant[field]
                for field in (
                    "pid",
                    "start_time_unix_s",
                    "executable",
                    "executable_bytes",
                    "executable_sha256",
                )
            }
            | {"input_ipc_server_argv": argv}
        )
    return result


def command_cleanup(args: argparse.Namespace) -> int:
    if os.name == "nt":
        raise ValueError("exact cleanup CLI is Unix-only")
    if not math.isfinite(args.timeout_secs) or args.timeout_secs <= 0:
        raise ValueError("--timeout-secs must be finite and positive")
    deadline = time.monotonic() + args.timeout_secs
    document: dict[str, Any] | None = None
    last_error: Exception | None = None
    while time.monotonic() <= deadline:
        try:
            if args.identity.is_file():
                document = load_json_object(args.identity)
                validated_live_identity(document, args.identity)
                break
        except (OSError, ValueError) as error:
            last_error = error
        time.sleep(0.02)
    if document is None:
        raise ValueError(
            f"no valid live identity appeared before cleanup deadline: {last_error or args.identity}"
        )
    producer, owner, _mpv_identities, descendant_identities = validated_live_identity(
        document, args.identity
    )
    partial_owner = document.get("partial_owner")
    if owner is None and isinstance(partial_owner, dict):
        observation = unix_process_observation(
            int(partial_owner["pid"]), hash_executable=True
        )
        if observation is not None:
            for field in ("pid", "start_time_unix_s", "process_group_id"):
                require_artifact_value(
                    args.identity,
                    f"partial owner {field}",
                    observation.get(field),
                    partial_owner[field],
                )
            require_artifact_value(
                args.identity,
                "partial owner parent PID",
                observation.get("parent_pid"),
                producer["pid"],
            )
            owner = observation

    def still_exact(
        identity: dict[str, Any], *, exact_command: bool = False, verify_hash: bool = False
    ) -> bool:
        return live_identity_matches(
            identity,
            unix_process_observation(
                int(identity["pid"]), hash_executable=verify_hash
            ),
            exact_command=exact_command,
            verify_hash=verify_hash,
        )

    targets = ([(owner, False)] if owner is not None else []) + [
        (item, True) for item in descendant_identities
    ]
    if document["state"] == "cleaned":
        while time.monotonic() < deadline and still_exact(producer, verify_hash=False):
            time.sleep(0.02)
        survivors = [
            identity["pid"]
            for identity, exact_command in [(producer, False), *targets]
            if still_exact(identity, exact_command=exact_command, verify_hash=True)
        ]
        if survivors:
            raise ValueError(f"{args.identity}: cleaned identity still has exact survivors {survivors}")
        require_artifact_value(args.identity, "cleanup proof", document.get("cleanup_proven"), True)
        print(json.dumps({"ok": True, "already_cleaned": True, "identity": str(args.identity)}))
        return 0

    stopped: list[dict[str, Any]] = []
    frozen_owner_group: int | None = None
    owner_group_killed = False

    def signal_exact(
        identity: dict[str, Any], requested_signal: signal.Signals, *, exact_command: bool = False
    ) -> bool:
        if not still_exact(identity, exact_command=exact_command, verify_hash=True):
            return False
        try:
            os.kill(int(identity["pid"]), requested_signal)
        except ProcessLookupError:
            return False
        return True

    cleanup_document: dict[str, Any] | None = None
    try:
        if signal_exact(producer, signal.SIGSTOP):
            stopped.append(producer)
        time.sleep(0.03)
        for identity in stopped:
            if not still_exact(identity, verify_hash=True):
                raise ValueError(
                    f"{args.identity}: exact PID {identity['pid']} changed while freezing cleanup writers/tree"
                )

        producer_group = int(producer["process_group_id"])
        if owner is None:
            relations = unix_process_relations()
            owner_candidates = sorted(
                pid
                for pid, (parent, process_group) in relations.items()
                if parent == int(producer["pid"])
                and pid == process_group
                and process_group != producer_group
                and pid != os.getpid()
            )
            if len(owner_candidates) > 1:
                raise ValueError(
                    f"{args.identity}: startup cleanup found multiple dedicated owner candidates "
                    f"{owner_candidates}"
                )
            if owner_candidates:
                candidate = unix_process_observation(
                    owner_candidates[0], hash_executable=True
                )
                if candidate is None:
                    raise ValueError(
                        f"{args.identity}: startup owner candidate disappeared before identity capture"
                    )
                require_artifact_value(
                    args.identity,
                    "startup owner parent PID",
                    candidate.get("parent_pid"),
                    producer["pid"],
                )
                owner = candidate

        if owner is not None:
            owner_observation = unix_process_observation(
                int(owner["pid"]), hash_executable=True
            )
            if live_identity_matches(owner, owner_observation, verify_hash=True):
                owner_group = int(owner_observation["process_group_id"])
                require_artifact_value(
                    args.identity,
                    "observed owner process group",
                    owner_group,
                    owner["process_group_id"],
                )
                if owner_group != int(owner["pid"]):
                    raise ValueError(
                        f"{args.identity}: exact owner PID {owner['pid']} is not leader of "
                        f"dedicated process group {owner_group}"
                    )
                if owner_group == producer_group:
                    raise ValueError(
                        f"{args.identity}: owner process group is not isolated from producer"
                    )
                if owner_group == os.getpgrp():
                    raise ValueError(
                        f"{args.identity}: refusing to freeze cleanup orchestrator process group"
                    )
                try:
                    os.killpg(owner_group, signal.SIGSTOP)
                except ProcessLookupError as error:
                    raise ValueError(
                        f"{args.identity}: dedicated owner process group {owner_group} disappeared "
                        "before atomic freeze"
                    ) from error
                frozen_owner_group = owner_group
                wait_for_process_group_stopped(owner_group, deadline)
                frozen_owner = unix_process_observation(
                    int(owner["pid"]), hash_executable=True
                )
                if not live_identity_matches(owner, frozen_owner, verify_hash=True):
                    raise ValueError(
                        f"{args.identity}: exact owner changed during process-group freeze"
                    )
                require_artifact_value(
                    args.identity,
                    "frozen owner process group",
                    frozen_owner.get("process_group_id"),
                    owner_group,
                )
            else:
                recorded_group = int(owner["process_group_id"])
                recorded_group_members = sorted(
                    pid
                    for pid, (_parent, process_group) in unix_process_relations().items()
                    if process_group == recorded_group
                )
                if recorded_group_members:
                    raise ValueError(
                        f"{args.identity}: cannot safely freeze recorded owner process group "
                        f"{recorded_group} without its exact leader; members={recorded_group_members}"
                    )

        # Keep every previously captured exact-alive descendant, including a grandchild that
        # was reparented after a short-lived intermediate exited.
        discovered: dict[tuple[int, int], dict[str, Any]] = {}
        for identity in descendant_identities:
            if still_exact(identity, exact_command=True, verify_hash=True):
                discovered[(identity["pid"], identity["start_time_unix_s"])] = identity

        stable_inventory: set[tuple[int, int]] | None = None
        stable_count = 0
        while time.monotonic() < deadline and stable_count < 3:
            relations = unix_process_relations()
            recursive = {int(producer["pid"])}
            if owner is not None:
                recursive.add(int(owner["pid"]))
            changed = True
            while changed:
                before = len(recursive)
                recursive.update(
                    pid for pid, (parent, _group) in relations.items() if parent in recursive
                )
                changed = len(recursive) != before
            reserved_pids = {int(producer["pid"]), os.getpid()}
            if owner is not None:
                reserved_pids.add(int(owner["pid"]))
            candidate_pids = recursive - reserved_pids
            if frozen_owner_group is not None:
                candidate_pids.update(
                    pid
                    for pid, (_parent, group) in relations.items()
                    if group == frozen_owner_group
                )
            candidate_pids -= reserved_pids
            for pid in sorted(candidate_pids):
                observation = unix_process_observation(pid, hash_executable=True)
                if observation is None:
                    continue
                identity = cleanup_descendant_identity(observation)
                discovered[(identity["pid"], identity["start_time_unix_s"])] = identity
                if signal_exact(identity, signal.SIGSTOP, exact_command=True):
                    stopped.append(identity)
            inventory = set(discovered)
            if inventory == stable_inventory:
                stable_count += 1
            else:
                stable_inventory = inventory
                stable_count = 1
            time.sleep(0.02)
        if stable_count < 3:
            raise ValueError(f"{args.identity}: recursive descendant inventory did not stabilize")
        if frozen_owner_group is not None:
            wait_for_process_group_stopped(frozen_owner_group, deadline)
        descendant_identities = sorted(
            discovered.values(), key=lambda identity: (identity["pid"], identity["start_time_unix_s"])
        )
        mpv_identities = mpv_identities_from_descendants(descendant_identities)
        cleanup_document = {
            **document,
            "state": "cleanup_requested",
            "owner": owner,
            "cleanup_scope": CLEANUP_SCOPE,
            "cleanup_proven": False,
            "descendants": descendant_identities,
            "mpv": mpv_identities,
            "cleanup_requested_unix_ns": time.time_ns(),
            "updated_unix_ns": time.time_ns(),
        }
        atomic_json(args.identity, cleanup_document)

        targets = ([(owner, False)] if owner is not None else []) + [
            (item, True) for item in descendant_identities
        ]
        if frozen_owner_group is not None:
            frozen_owner = (
                unix_process_observation(int(owner["pid"]), hash_executable=True)
                if owner is not None
                else None
            )
            if owner is None or not live_identity_matches(
                owner, frozen_owner, verify_hash=True
            ):
                raise ValueError(
                    f"{args.identity}: exact owner changed before dedicated process-group kill"
                )
            require_artifact_value(
                args.identity,
                "owner process group before kill",
                frozen_owner.get("process_group_id"),
                frozen_owner_group,
            )
            try:
                os.killpg(frozen_owner_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
            owner_group_killed = True
        for identity, exact_command in reversed(targets):
            signal_exact(identity, signal.SIGKILL, exact_command=exact_command)
        signal_exact(producer, signal.SIGKILL)
        while time.monotonic() < deadline and any(
            still_exact(identity, exact_command=exact_command)
            for identity, exact_command in [(producer, False), *targets]
        ):
            time.sleep(0.05)
    finally:
        if frozen_owner_group is not None and not owner_group_killed and owner is not None:
            current_owner = unix_process_observation(
                int(owner["pid"]), hash_executable=True
            )
            if live_identity_matches(owner, current_owner, verify_hash=True) and (
                current_owner.get("process_group_id") == frozen_owner_group
            ):
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(frozen_owner_group, signal.SIGCONT)
        for identity in stopped:
            signal_exact(identity, signal.SIGCONT)

    targets = ([(owner, False)] if owner is not None else []) + [
        (item, True) for item in descendant_identities
    ]
    survivors = [
        identity["pid"]
        for identity, exact_command in [(producer, False), *targets]
        if still_exact(identity, exact_command=exact_command, verify_hash=True)
    ]
    if survivors:
        raise ValueError(f"{args.identity}: exact processes survived cleanup {survivors}")
    if cleanup_document is None:
        raise ValueError(f"{args.identity}: cleanup request was not durably published")
    cleaned = {
        **cleanup_document,
        "state": "cleaned",
        "cleanup_scope": CLEANUP_SCOPE,
        "cleanup_proven": True,
        "cleanup_method": "verified_unix_dedicated_process_group_and_observed_exact_descendants",
        "updated_unix_ns": time.time_ns(),
    }
    atomic_json(args.identity, cleaned)
    print(json.dumps({"ok": True, "already_cleaned": False, "identity": str(args.identity)}))
    return 0


def validate_launch_policy(path: Path, run_root: Path) -> tuple[dict[str, Any], list[Path]]:
    manifest = load_json_object(path)
    require_artifact_value(path, "schema", manifest.get("schema"), "ytt.tui-perf.launch-policy.v1")
    home = (run_root / "home").resolve()
    require_artifact_value(path, "root", Path(str(manifest.get("root", ""))).resolve(), home)
    require_artifact_value(path, "config path", manifest.get("config"), "stores/config/config.json")
    require_artifact_value(
        path,
        "effective launch policy",
        manifest.get("effective"),
        LAUNCH_POLICY_EFFECTIVE,
    )
    expected_policy_fields = [".".join(fields) for fields in LAUNCH_POLICY_FIELDS]
    require_artifact_value(
        path, "launch policy field inventory", manifest.get("policy_fields"), expected_policy_fields
    )
    snapshot = run_root / "launch-policy-inputs"
    require_artifact_value(path, "snapshot", manifest.get("snapshot"), snapshot.name)
    require_artifact_value(
        path, "snapshot policy path", manifest.get("snapshot_policy"), "effective-policy.json"
    )
    snapshot_policy = snapshot / "effective-policy.json"
    if not snapshot_policy.is_file():
        raise ValueError(f"{path}: launch policy effective snapshot is missing")
    snapshot_document = load_json_object(snapshot_policy)
    require_artifact_value(
        snapshot_policy,
        "schema",
        snapshot_document.get("schema"),
        "ytt.tui-perf.launch-policy-input.v1",
    )
    require_artifact_value(
        snapshot_policy,
        "effective launch policy",
        snapshot_document.get("effective"),
        LAUNCH_POLICY_EFFECTIVE,
    )
    require_artifact_value(
        snapshot_policy,
        "launch policy field inventory",
        snapshot_document.get("policy_fields"),
        expected_policy_fields,
    )
    require_artifact_value(
        path,
        "child environment policy",
        manifest.get("child_environment_policy"),
        CHILD_ENVIRONMENT_POLICY,
    )
    require_artifact_value(
        snapshot_policy,
        "child environment policy",
        snapshot_document.get("child_environment_policy"),
        CHILD_ENVIRONMENT_POLICY,
    )
    config = (home / "stores" / "config" / "config.json").resolve()
    try:
        config.relative_to(home)
    except ValueError as error:
        raise ValueError(f"{path}: live launch config escapes the isolated home") from error
    if not config.is_file():
        raise ValueError(f"{path}: live launch policy config is missing")
    document = load_json_object(config)
    validate_effective_launch_config(config, document)
    credential_proof = launch_api_credential_proof(document, home)
    require_artifact_value(
        path, "API credential absence proof", manifest.get("api_credential_proof"), credential_proof
    )
    require_artifact_value(
        snapshot_policy,
        "API credential absence proof",
        snapshot_document.get("api_credential_proof"),
        credential_proof,
    )
    require_artifact_value(
        path,
        "preserved config projection SHA-256",
        manifest.get("preserved_config_projection_sha256"),
        json_value_sha256(launch_policy_projection(document)),
    )
    recorded_config = manifest.get("config_after")
    if not isinstance(recorded_config, dict):
        raise ValueError(f"{path}: config_after identity is malformed")
    live_identity = identity_for_file(config)
    require_artifact_value(
        path,
        "config content identity",
        recorded_config,
        live_identity,
    )
    require_artifact_value(path, "snapshot inventory", manifest.get("snapshot_files"), tree_file_inventory(snapshot))
    return manifest, [path, snapshot_policy]


def launch_policy_self_test() -> None:
    def expect_path_rejected(
        base: Path,
        name: str,
        output_for: Any,
        setup: Any = None,
    ) -> None:
        run_root = base / name
        home = run_root / "home"
        config = home / "stores" / "config" / "config.json"
        config.parent.mkdir(parents=True)
        atomic_json(config, {"unrelated": {"preserve": "byte-identical"}})
        if setup is not None:
            setup(run_root)
        output = output_for(run_root, home)
        output_existed = path_entry_exists(output)
        before = byte_exact_tree_state_for_self_test(home)
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                command_launch_policy(argparse.Namespace(root=home, output=output))
        except ValueError:
            pass
        else:
            raise AssertionError(f"launch policy accepted invalid {name} paths")
        if byte_exact_tree_state_for_self_test(home) != before:
            raise AssertionError(
                f"launch policy invalid {name} paths changed input-tree bytes or shape"
            )
        if not output_existed and path_entry_exists(output):
            raise AssertionError(f"rejected launch policy created invalid {name} output")

    def expect_rejected(
        base: Path,
        name: str,
        document: dict[str, Any],
        setup: Any = None,
    ) -> None:
        run_root = base / name
        home = run_root / "home"
        config = home / "stores" / "config" / "config.json"
        config.parent.mkdir(parents=True)
        atomic_json(config, document)
        if setup is not None:
            setup(home)
        before = config.read_bytes()
        output = run_root / "launch-policy.json"
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                command_launch_policy(argparse.Namespace(root=home, output=output))
        except ValueError as error:
            if "self-test-secret" in str(error):
                raise AssertionError("launch-policy rejection leaked a cookie value") from error
        else:
            raise AssertionError(f"launch policy accepted hostile seed {name}")
        if config.read_bytes() != before:
            raise AssertionError("rejected launch policy mutated its source config")
        if output.exists() or (run_root / "launch-policy-inputs").exists():
            raise AssertionError("rejected launch policy archived an untrusted config")

    with tempfile.TemporaryDirectory(prefix="ytt-perf-launch-policy-self-test-") as temporary:
        base = Path(temporary)

        expect_path_rejected(
            base,
            "output-inside-home",
            lambda _run_root, home: home / "artifacts" / "launch-policy.json",
        )

        def occupy_snapshot(run_root: Path) -> None:
            (run_root / "launch-policy-inputs").mkdir()

        expect_path_rejected(
            base,
            "occupied-snapshot",
            lambda run_root, _home: run_root / "launch-policy.json",
            occupy_snapshot,
        )
        expect_path_rejected(
            base,
            "output-snapshot-alias",
            lambda run_root, _home: run_root / "launch-policy-inputs",
        )

        run_root = base / "valid"
        home = run_root / "home"
        config = home / "stores" / "config" / "config.json"
        config.parent.mkdir(parents=True)
        secrets_by_path = {
            "cookie": "PREF=self-test-secret-nonauth-cookie",
            "gemini_api_key": "self-test-secret-gemini",
            "lastfm_session": "self-test-secret-lastfm-session",
            "lastfm_api_secret": "self-test-secret-lastfm-api",
            "listenbrainz_token": "self-test-secret-listenbrainz",
        }
        hostile = {
            "cookie": secrets_by_path["cookie"],
            "cookies_file": None,
            "gemini_api_key": secrets_by_path["gemini_api_key"],
            "media_controls": True,
            "update_check_enabled": True,
            "autoplay_on_start": True,
            "autoplay_streaming": True,
            "album_art": True,
            "romanized_titles": True,
            "ai_enabled": True,
            "tools": {"ytdlp_managed": True, "ytdlp_channel": "stable"},
            "scrobble": {
                "lastfm": {
                    "enabled": True,
                    "session_key": secrets_by_path["lastfm_session"],
                    "api_secret": secrets_by_path["lastfm_api_secret"],
                    "username": "self-test-user",
                    "love_sync": True,
                },
                "listenbrainz": {
                    "enabled": True,
                    "token": secrets_by_path["listenbrainz_token"],
                    "api_url": "https://listen.example.invalid",
                },
                "local_files": True,
            },
            "unrelated": {"nested": [1, "keep-me"]},
        }
        atomic_json(config, hostile)
        original_projection = launch_policy_projection(hostile)
        output = run_root / "launch-policy.json"
        with contextlib.redirect_stdout(io.StringIO()):
            command_launch_policy(argparse.Namespace(root=home, output=output))
        frozen = load_json_object(config)
        validate_effective_launch_config(config, frozen)
        if launch_policy_projection(frozen) != original_projection:
            raise AssertionError("launch policy changed a secret or unrelated config field")
        if frozen["scrobble"]["lastfm"]["session_key"] != secrets_by_path["lastfm_session"]:
            raise AssertionError("launch policy changed the Last.fm session secret")
        if frozen["scrobble"]["listenbrainz"]["token"] != secrets_by_path["listenbrainz_token"]:
            raise AssertionError("launch policy changed the ListenBrainz token")
        validate_launch_policy(output, run_root)
        policy_artifacts = [
            output,
            run_root / "launch-policy-inputs" / "effective-policy.json",
        ]
        serialized_policy = "\n".join(
            artifact.read_text(encoding="utf-8") for artifact in policy_artifacts
        )
        for secret in secrets_by_path.values():
            if secret in serialized_policy:
                raise AssertionError("launch policy artifact contains a raw secret value")

        snapshot_policy = policy_artifacts[1]
        original_snapshot = snapshot_policy.read_bytes()
        tampered = load_json_object(snapshot_policy)
        tampered["effective"] = {**LAUNCH_POLICY_EFFECTIVE, "media_controls": True}
        atomic_json(snapshot_policy, tampered)
        try:
            validate_launch_policy(output, run_root)
        except ValueError:
            pass
        else:
            raise AssertionError("launch policy validator accepted a tampered effective snapshot")
        atomic_bytes(snapshot_policy, original_snapshot)
        validate_launch_policy(output, run_root)

        expect_rejected(
            base,
            "inline-cookie",
            {"cookie": "SAPISID=self-test-secret-inline"},
        )
        expect_rejected(base, "explicit-empty-path", {"cookies_file": ""})

        def default_cookie(home_root: Path) -> None:
            cookie = home_root / "Music" / "yututui" / "cookies.txt"
            cookie.parent.mkdir(parents=True)
            cookie.write_text(
                ".youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tself-test-secret-file\n",
                encoding="utf-8",
            )

        expect_rejected(base, "default-cookie", {}, default_cookie)

        def user_dirs_override(home_root: Path) -> None:
            user_dirs = home_root / ".config" / "user-dirs.dirs"
            user_dirs.parent.mkdir(parents=True)
            user_dirs.write_text('XDG_MUSIC_DIR="/outside-isolated-home"\n', encoding="utf-8")

        expect_rejected(base, "xdg-user-dirs-override", {}, user_dirs_override)


def child_environment_policy_self_test() -> None:
    shell_path = Path(__file__).resolve().with_suffix(".sh")
    shell = shell_path.read_text(encoding="utf-8")
    try:
        isolated_block = shell.split("  local -a isolated_env=(", 1)[1].split("\n  )", 1)[0]
    except IndexError as error:
        raise AssertionError("cannot locate the isolated child environment in tui-perf.sh") from error
    for required in (
        '"PATH=$PATH"',
        '"HOME=$home"',
        '"XDG_CONFIG_HOME=$home/.config"',
        '"XDG_DATA_HOME=$home/.local/share"',
        '"XDG_CACHE_HOME=$home/.cache"',
        '"XDG_STATE_HOME=$home/.local/state"',
        '"XDG_RUNTIME_DIR=$runtime"',
        '"YTM_CONFIG_DIR=$config_store"',
        '"YTM_DATA_DIR=$data_store"',
        '"YTM_CACHE_DIR=$cache_store"',
        '"TMPDIR=$tmp"',
        '"TEMP=$tmp"',
        '"TMP=$tmp"',
        '"TERM=xterm-256color"',
        '"YTM_MPV_EXTRA=--ao=null --volume=0 --audio-display=no"',
        '"TUI_PERF_SCENARIO_SHA256=$scenario_hash"',
        '"TUI_PERF_RUN_ID=$run_id"',
    ):
        if required not in isolated_block:
            raise AssertionError(f"isolated child environment is missing {required}")
    for forbidden in ('"GEMINI_API_KEY=', '"YTM_PLAY_URL=', '"YTM_PERF='):
        if forbidden in isolated_block:
            raise AssertionError(f"isolated child environment inherited {forbidden}")
    for required_command in (
        'env -i "${isolated_env[@]}" "$sampler"',
        'env -i "${isolated_env[@]}" "$controller"',
    ):
        if required_command not in shell:
            raise AssertionError(f"child process does not use empty-environment launch: {required_command}")

    ambient = {
        **os.environ,
        "GEMINI_API_KEY": "hostile-ambient-value",
        "YTM_PLAY_URL": "https://remote.example.invalid/must-not-survive",
        "YTM_PERF": "1",
    }
    preserved_path = os.environ.get("PATH", "")
    probe = subprocess.run(
        [
            "env",
            "-i",
            f"PATH={preserved_path}",
            "LANG=C",
            sys.executable,
            "-c",
            (
                "import json,os; "
                "print(json.dumps({k:os.environ.get(k) for k in "
                "['PATH','LANG','GEMINI_API_KEY','YTM_PLAY_URL','YTM_PERF']}))"
            ),
        ],
        env=ambient,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if probe.returncode != 0:
        raise AssertionError(f"env -i policy probe failed: {probe.stderr.strip()}")
    observed = json.loads(probe.stdout)
    require_artifact_value(shell_path, "child PATH passthrough", observed["PATH"], preserved_path)
    require_artifact_value(shell_path, "child LANG passthrough", observed["LANG"], "C")
    for key in ("GEMINI_API_KEY", "YTM_PLAY_URL", "YTM_PERF"):
        if observed[key] is not None:
            raise AssertionError(f"env -i policy retained hostile ambient key {key}")


def command_fixture(args: argparse.Namespace) -> int:
    if args.seconds <= 0 or args.sample_rate <= 0:
        raise ValueError("fixture duration and sample rate must be positive")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    frames = int(round(args.seconds * args.sample_rate))
    chunk_frames = min(args.sample_rate, 65_536)
    silence = b"\0\0" * chunk_frames
    with wave.open(str(args.output), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(args.sample_rate)
        remaining = frames
        while remaining:
            count = min(remaining, chunk_frames)
            wav.writeframesraw(silence[: count * 2])
            remaining -= count
    manifest = {
        "schema": "ytt.tui-perf.fixture.v1",
        "path": str(args.output.resolve()),
        "seconds": args.seconds,
        "sample_rate_hz": args.sample_rate,
        "channels": 1,
        "sample_width_bytes": 2,
        "frames": frames,
        "bytes": args.output.stat().st_size,
        "sha256": sha256_file(args.output),
    }
    if args.manifest:
        atomic_json(args.manifest, manifest)
    print(json.dumps(manifest, sort_keys=True))
    return 0


def command_check(args: argparse.Namespace) -> int:
    records = read_ndjson(args.samples)
    errors = [record for record in records if record.get("kind") == "error"]
    if errors:
        raise ValueError(f"sampler reported: {errors[-1].get('message')}")
    headers = [record for record in records if record.get("kind") == "header"]
    summaries = [record for record in records if record.get("kind") == "summary"]
    measured = [
        record
        for record in records
        if record.get("kind") == "sample" and record.get("phase") == "measure"
    ]
    if len(headers) != 1 or len(summaries) != 1 or not measured:
        raise ValueError("samples need exactly one header/summary and at least one measured sample")
    if args.scenario_sha256 and headers[0].get("scenario_sha256") != args.scenario_sha256:
        raise ValueError("sample scenario SHA-256 does not match the selected scenario file")
    if args.require_silent_mpv:
        if not summaries[0].get("silent_mpv_proven"):
            raise ValueError("sampler did not prove null audio and zero volume from measured argv")
        if any(
            record.get("mpv_present") is not True
            or record.get("mpv_all_silent_this_sample") is not True
            for record in measured
        ):
            raise ValueError("every measured sample must contain only effective null/zero mpv argv")
        identities = summaries[0].get("last_observed_mpv")
        if not isinstance(identities, list) or not identities:
            raise ValueError("sampler did not retain an exact measured mpv cleanup identity")
        for identity in identities:
            if not (
                isinstance(identity, dict)
                and isinstance(identity.get("pid"), int)
                and isinstance(identity.get("start_time_unix_s"), int)
                and isinstance(identity.get("input_ipc_server_argv"), list)
                and identity["input_ipc_server_argv"]
            ):
                raise ValueError("sampler mpv cleanup identity is incomplete")
        validate_measured_samples(
            args.samples,
            headers[0],
            summaries[0],
            [record for record in records if record.get("kind") == "sample"],
            True,
            REQUIRED_PLAYBACK_MPV_CACHE_ARGS,
        )
    if args.control:
        control = read_ndjson(args.control)
        control_summaries = [record for record in control if record.get("kind") == "summary"]
        if len(control_summaries) != 1:
            raise ValueError("control output needs exactly one summary")
        if (
            args.require_observer_close
            and control_summaries[0].get("observation_end") != "mpv_ipc_closed"
        ):
            raise ValueError("controller did not observe the sampler-owned mpv IPC closing")
        control_headers = [record for record in control if record.get("kind") == "header"]
        if len(control_headers) != 1:
            raise ValueError("control output needs exactly one header")
        if args.scenario_sha256 and control_headers[0].get("scenario_sha256") != args.scenario_sha256:
            raise ValueError("control scenario SHA-256 does not match the selected scenario file")
        require_artifact_value(
            args.control,
            "controller buffering cutoff",
            control_headers[0].get("buffering_cutoff_ns"),
            control_summaries[0].get("buffering_cutoff_ns"),
        )
        require_artifact_value(
            args.control,
            "controller buffering cutoff/observation duration",
            control_headers[0].get("buffering_cutoff_ns"),
            control_headers[0].get("observe_ns"),
        )
        validate_control_buffering(args.control, control, control_summaries[0])
        validate_control_time_pos_summary(args.control, control, control_summaries[0])
    print(json.dumps({"ok": True, "measured_samples": len(measured)}))
    return 0


class FixtureServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int], handler: type[http.server.BaseHTTPRequestHandler],
                 file: Path, throttle_bps: int, outage_every_bytes: int, outage_ms: int,
                 disconnect_every_bytes: int, request_log: Path, run_id: str,
                 shutdown_token: str | None = None):
        super().__init__(address, handler)
        self.fixture_file = file
        self.fixture_sha256 = sha256_file(file)
        self.run_id = run_id
        self.shutdown_token = shutdown_token
        self.request_log = request_log.resolve()
        if self.request_log.exists():
            raise ValueError("--request-log must name a new path")
        self.request_log.parent.mkdir(parents=True, exist_ok=True)
        self.request_log.touch()
        self.throttle_bps = throttle_bps
        self.outage_every_bytes = outage_every_bytes
        self.outage_ms = outage_ms
        self.disconnect_every_bytes = disconnect_every_bytes
        self.transfer_lock = threading.Lock()
        self.log_lock = threading.Lock()
        self.request_counter = 0
        self.total_socket_bytes_accepted = 0
        self.next_outage = outage_every_bytes if outage_every_bytes else 0
        self.next_disconnect = disconnect_every_bytes if disconnect_every_bytes else 0
        self.pacing_origin_monotonic_ns = time.monotonic_ns()
        # Zero initial credit: the first throttled chunk reserves its full byte-time before
        # any body byte is written.  One shared deadline makes concurrent Range connections
        # consume the same aggregate budget rather than receiving a budget per handler.
        self.next_byte_deadline_monotonic_ns = self.pacing_origin_monotonic_ns

    def next_request_id(self) -> int:
        with self.log_lock:
            self.request_counter += 1
            return self.request_counter

    def log_request(self, record: dict[str, Any]) -> None:
        value = {
            "schema": "ytt.tui-perf.http-request.v1",
            "run_id": self.run_id,
            "server_pid": os.getpid(),
            "fixture_sha256": self.fixture_sha256,
            **record,
        }
        encoded = json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
        with self.log_lock:
            with self.request_log.open("a", encoding="utf-8") as stream:
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())

    def reserve_pacing_deadline(self, byte_count: int) -> tuple[int, int | None]:
        reserved_ns = time.monotonic_ns()
        if not self.throttle_bps:
            return reserved_ns, None
        byte_time_ns = (byte_count * 1_000_000_000 + self.throttle_bps - 1) // self.throttle_bps
        deadline_ns = max(self.next_byte_deadline_monotonic_ns, reserved_ns) + byte_time_ns
        self.next_byte_deadline_monotonic_ns = deadline_ns
        return reserved_ns, deadline_ns

    @staticmethod
    def wait_for_pacing_deadline(deadline_ns: int | None) -> None:
        while deadline_ns is not None:
            remaining_ns = deadline_ns - time.monotonic_ns()
            if remaining_ns <= 0:
                return
            time.sleep(remaining_ns / 1_000_000_000)


class RangeFixtureHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    @property
    def perf_server(self) -> FixtureServer:
        return self.server  # type: ignore[return-value]

    def do_HEAD(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        self._serve(send_body=False)

    def do_GET(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        self._serve(send_body=True)

    def do_POST(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        server = self.perf_server
        path = self.path.split("?", 1)[0]
        authorization = self.headers.get("Authorization", "")
        requested_run_id = self.headers.get("X-Ytt-Tui-Perf-Run-Id", "")
        expected_authorization = (
            f"Bearer {server.shutdown_token}" if server.shutdown_token is not None else ""
        )
        authorized = (
            path == HTTP_SHUTDOWN_PATH
            and server.shutdown_token is not None
            and secrets.compare_digest(authorization, expected_authorization)
            and secrets.compare_digest(requested_run_id, server.run_id)
            and self.headers.get("Content-Length") in {None, "0"}
        )
        if not authorized:
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            return
        token_sha256 = hashlib.sha256(server.shutdown_token.encode("ascii")).hexdigest()
        payload = json.dumps(
            {
                "schema": HTTP_SHUTDOWN_SCHEMA,
                "run_id": server.run_id,
                "pid": os.getpid(),
                "token_sha256": token_sha256,
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()
        self.close_connection = True
        threading.Thread(
            target=server.shutdown,
            name="ytt-tui-perf-http-shutdown",
            daemon=True,
        ).start()

    def _serve(self, send_body: bool) -> None:
        server = self.perf_server
        request_id = server.next_request_id()
        path = self.path.split("?", 1)[0]
        method = "GET" if send_body else "HEAD"
        started_ns = time.monotonic_ns()
        if path != "/fixture.wav":
            server.log_request({
                "kind": "request_terminal", "request_id": request_id, "method": method,
                "path": path, "range_header": self.headers.get("Range"), "status": 404,
                "planned_start": None, "planned_end": None, "bytes_planned": 0,
                "server_socket_bytes_accepted": 0,
                "started_monotonic_ns": started_ns,
                "finished_monotonic_ns": time.monotonic_ns(), "terminal_reason": "unexpected_path",
                "outage_or_disconnect_action": None,
            })
            self.send_error(404)
            return
        size = server.fixture_file.stat().st_size
        try:
            start, end, partial = parse_range(self.headers.get("Range"), size)
        except ValueError:
            server.log_request({
                "kind": "request_terminal", "request_id": request_id, "method": method,
                "path": path, "range_header": self.headers.get("Range"), "status": 416,
                "planned_start": None, "planned_end": None, "bytes_planned": 0,
                "server_socket_bytes_accepted": 0,
                "started_monotonic_ns": started_ns,
                "finished_monotonic_ns": time.monotonic_ns(), "terminal_reason": "invalid_range",
                "outage_or_disconnect_action": None,
            })
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        length = end - start + 1
        status = 206 if partial else 200
        server.log_request({
            "kind": "request_started", "request_id": request_id, "method": method,
            "path": path, "range_header": self.headers.get("Range"), "status": status,
            "planned_start": start, "planned_end": end, "bytes_planned": length,
            "server_socket_bytes_accepted": 0,
            "started_monotonic_ns": started_ns,
            "finished_monotonic_ns": None, "terminal_reason": None,
            "outage_or_disconnect_action": None,
        })
        self.send_response(status)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(length))
        self.send_header("ETag", '"ytt-perf-silence-v1"')
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if not send_body:
            server.log_request({
                "kind": "request_terminal", "request_id": request_id, "method": method,
                "path": path, "range_header": self.headers.get("Range"), "status": status,
                "planned_start": start, "planned_end": end, "bytes_planned": length,
                "server_socket_bytes_accepted": 0,
                "started_monotonic_ns": started_ns,
                "finished_monotonic_ns": time.monotonic_ns(), "terminal_reason": "head_complete",
                "outage_or_disconnect_action": None,
            })
            return

        remaining = length
        delivered = 0
        terminal_reason = "complete"
        with server.fixture_file.open("rb") as stream:
            stream.seek(start)
            while remaining:
                with server.transfer_lock:
                    max_chunk = HTTP_THROTTLE_CHUNK_BYTES if server.throttle_bps else 64 * 1024
                    limit = min(max_chunk, remaining)
                    thresholds = [
                        threshold - server.total_socket_bytes_accepted
                        for threshold in (server.next_outage, server.next_disconnect)
                        if threshold and threshold > server.total_socket_bytes_accepted
                    ]
                    if thresholds:
                        limit = min(limit, min(thresholds))
                    chunk = stream.read(limit)
                    if not chunk:
                        terminal_reason = "fixture_eof"
                        break
                    pacing_reserved_ns, pacing_deadline_ns = server.reserve_pacing_deadline(
                        len(chunk)
                    )
                    server.wait_for_pacing_deadline(pacing_deadline_ns)
                    write_started_ns = time.monotonic_ns()
                    try:
                        self.wfile.write(chunk)
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        # No body bytes were delivered.  Because this handler still owns the
                        # global transfer lock, discard the failed reservation without granting
                        # future credit; the next successful chunk will wait its complete time.
                        if server.throttle_bps:
                            server.next_byte_deadline_monotonic_ns = time.monotonic_ns()
                        terminal_reason = "client_disconnect"
                        break
                    write_finished_ns = time.monotonic_ns()
                    remaining -= len(chunk)
                    delivered += len(chunk)
                    server.total_socket_bytes_accepted += len(chunk)
                    actions: list[dict[str, Any]] = []
                    if (
                        server.next_outage
                        and server.total_socket_bytes_accepted >= server.next_outage
                    ):
                        actions.append({"action": "outage", "threshold": server.next_outage})
                        server.next_outage += server.outage_every_bytes
                    if (
                        server.next_disconnect
                        and server.total_socket_bytes_accepted >= server.next_disconnect
                    ):
                        actions.append({"action": "disconnect", "threshold": server.next_disconnect})
                        server.next_disconnect += server.disconnect_every_bytes
                    server.log_request({
                        "kind": "delivery", "request_id": request_id, "method": method,
                        "path": path, "range_header": self.headers.get("Range"), "status": status,
                        "planned_start": start, "planned_end": end, "bytes_planned": length,
                        "server_socket_bytes_accepted": len(chunk),
                        "request_server_socket_bytes_accepted": delivered,
                        "global_server_socket_bytes_accepted": (
                            server.total_socket_bytes_accepted
                        ),
                        "started_monotonic_ns": started_ns,
                        "finished_monotonic_ns": time.monotonic_ns(), "terminal_reason": None,
                        "pacing_reserved_monotonic_ns": pacing_reserved_ns,
                        "pacing_deadline_monotonic_ns": pacing_deadline_ns,
                        "write_started_monotonic_ns": write_started_ns,
                        "write_finished_monotonic_ns": write_finished_ns,
                        "outage_or_disconnect_action": actions or None,
                    })
                    if any(action["action"] == "outage" for action in actions):
                        time.sleep(server.outage_ms / 1000.0)
                    if any(action["action"] == "disconnect" for action in actions):
                        terminal_reason = "planned_disconnect"
                        self.close_connection = True
                        remaining = 0
                        break
        server.log_request({
            "kind": "request_terminal", "request_id": request_id, "method": method,
            "path": path, "range_header": self.headers.get("Range"), "status": status,
            "planned_start": start, "planned_end": end, "bytes_planned": length,
            "server_socket_bytes_accepted": delivered,
            "started_monotonic_ns": started_ns,
            "finished_monotonic_ns": time.monotonic_ns(), "terminal_reason": terminal_reason,
            "outage_or_disconnect_action": None,
        })

    def log_message(self, fmt: str, *values: Any) -> None:
        if getattr(self.server, "verbose", False):
            super().log_message(fmt, *values)


def parse_range(header: str | None, size: int) -> tuple[int, int, bool]:
    if not header:
        return 0, size - 1, False
    if not header.startswith("bytes=") or "," in header:
        raise ValueError("unsupported range")
    first, last = header[6:].split("-", 1)
    if not first:
        suffix = int(last)
        if suffix <= 0:
            raise ValueError("invalid suffix")
        return max(0, size - suffix), size - 1, True
    start = int(first)
    end = int(last) if last else size - 1
    if start < 0 or start >= size or end < start:
        raise ValueError("range outside file")
    return start, min(end, size - 1), True


def exact_cli_argument_values(command: list[str], flag: str) -> list[str]:
    values: list[str] = []
    index = 0
    while index < len(command):
        argument = command[index]
        if argument == flag:
            if index + 1 >= len(command):
                raise ValueError(f"exact command has a terminal {flag}")
            values.append(command[index + 1])
            index += 2
            continue
        prefix = f"{flag}="
        if argument.startswith(prefix):
            values.append(argument[len(prefix) :])
        index += 1
    return values


def validate_fixture_server_manifest(
    path: Path, document: dict[str, Any], expected_run_id: str | None
) -> tuple[dict[str, Any], dict[str, Any]]:
    require_artifact_value(path, "HTTP schema", document.get("schema"), "ytt.tui-perf.http.v1")
    run_id = document.get("run_id")
    if not isinstance(run_id, str) or not run_id:
        raise ValueError(f"{path}: HTTP run_id is missing")
    if expected_run_id is not None:
        require_artifact_value(path, "HTTP shutdown run_id", run_id, expected_run_id)
    pid = non_negative_integer(document.get("pid"), "HTTP server PID", path)
    if pid == 0:
        raise ValueError(f"{path}: HTTP server PID must be positive")
    host = document.get("host")
    if not isinstance(host, str):
        raise ValueError(f"{path}: HTTP server host is malformed")
    try:
        host_address = ipaddress.ip_address(host)
    except ValueError as error:
        raise ValueError(f"{path}: HTTP server host is malformed") from error
    if not host_address.is_loopback:
        raise ValueError(f"{path}: HTTP shutdown endpoint is not loopback")
    port = non_negative_integer(document.get("port"), "HTTP server port", path)
    if port <= 0 or port > 65535:
        raise ValueError(f"{path}: HTTP server port is outside 1..65535")
    require_artifact_value(
        path,
        "HTTP fixture URL",
        document.get("url"),
        f"http://{host}:{port}/fixture.wav",
    )

    shutdown = document.get("shutdown")
    if not isinstance(shutdown, dict):
        raise ValueError(f"{path}: authenticated HTTP shutdown identity is missing")
    require_artifact_value(path, "HTTP shutdown schema", shutdown.get("schema"), HTTP_SHUTDOWN_SCHEMA)
    require_artifact_value(path, "HTTP shutdown method", shutdown.get("method"), HTTP_SHUTDOWN_METHOD)
    require_artifact_value(path, "HTTP shutdown path", shutdown.get("path"), HTTP_SHUTDOWN_PATH)
    token = shutdown.get("token")
    if not isinstance(token, str) or re.fullmatch(r"[A-Za-z0-9_-]{43,128}", token) is None:
        raise ValueError(f"{path}: HTTP shutdown token is malformed")
    token_sha256 = hashlib.sha256(token.encode("ascii")).hexdigest()
    require_artifact_value(
        path, "HTTP shutdown token SHA-256", shutdown.get("token_sha256"), token_sha256
    )
    require_artifact_value(path, "HTTP shutdown run_id binding", shutdown.get("run_id"), run_id)

    identity = document.get("server_process")
    if not isinstance(identity, dict):
        raise ValueError(f"{path}: exact HTTP server process identity is missing")
    require_artifact_value(
        path,
        "HTTP server process identity schema",
        identity.get("schema"),
        "ytt.tui-perf.http-process.v1",
    )
    require_artifact_value(path, "HTTP server identity PID", identity.get("pid"), pid)
    for field in ("start_time_unix_s", "executable_bytes"):
        if non_negative_integer(identity.get(field), f"HTTP server {field}", path) <= 0:
            raise ValueError(f"{path}: HTTP server {field} must be positive")
    native_start_token = identity.get("native_start_token")
    executable = identity.get("executable")
    executable_sha256 = identity.get("executable_sha256")
    command = identity.get("command")
    if (
        not isinstance(native_start_token, str)
        or not native_start_token
        or not isinstance(executable, str)
        or not executable
        or not isinstance(executable_sha256, str)
        or re.fullmatch(r"[0-9a-f]{64}", executable_sha256) is None
        or not isinstance(command, list)
        or not command
        or not all(isinstance(item, str) for item in command)
    ):
        raise ValueError(f"{path}: exact HTTP server executable/argv identity is malformed")
    if exact_cli_argument_values(command, "--run-id") != [run_id]:
        raise ValueError(f"{path}: exact HTTP server argv does not bind its run_id")
    if exact_cli_argument_values(command, "--shutdown-token") != [token]:
        raise ValueError(f"{path}: exact HTTP server argv does not bind its shutdown token")
    if command.count("serve") != 1:
        raise ValueError(f"{path}: exact HTTP server argv does not bind the serve subcommand")
    return identity, shutdown


def stop_fixture_server(
    identity_path: Path, expected_run_id: str | None, timeout_secs: float
) -> dict[str, Any]:
    if os.name == "nt":
        raise ValueError("exact fixture server shutdown CLI is Unix-only")
    if not math.isfinite(timeout_secs) or timeout_secs <= 0:
        raise ValueError("--timeout-secs must be finite and positive")
    document = load_json_object(identity_path)
    identity, shutdown = validate_fixture_server_manifest(
        identity_path, document, expected_run_id
    )
    observation = fixture_server_process_observation(int(identity["pid"]))
    if not fixture_server_identity_matches(identity, observation):
        return {
            "ok": True,
            "already_stopped": True,
            "identity": str(identity_path),
            "reason": "exact_process_identity_not_live",
        }

    deadline = time.monotonic() + timeout_secs
    remaining = max(0.05, deadline - time.monotonic())
    connection = http.client.HTTPConnection(
        str(document["host"]), int(document["port"]), timeout=min(2.0, remaining)
    )
    try:
        connection.request(
            "POST",
            str(shutdown["path"]),
            body=b"",
            headers={
                "Authorization": f"Bearer {shutdown['token']}",
                "Content-Length": "0",
                "X-Ytt-Tui-Perf-Run-Id": str(document["run_id"]),
            },
        )
        response = connection.getresponse()
        payload_bytes = response.read()
    except (OSError, http.client.HTTPException) as error:
        current = fixture_server_process_observation(int(identity["pid"]))
        if not fixture_server_identity_matches(identity, current):
            return {
                "ok": True,
                "already_stopped": True,
                "identity": str(identity_path),
                "reason": "exact_process_exited_before_shutdown_response",
            }
        raise ValueError(
            f"{identity_path}: authenticated fixture shutdown request failed: {error}"
        ) from error
    finally:
        connection.close()
    if response.status != 200:
        raise ValueError(
            f"{identity_path}: authenticated fixture shutdown returned HTTP {response.status}"
        )
    try:
        payload = json.loads(
            payload_bytes.decode("utf-8"), object_pairs_hook=reject_duplicate_json_keys
        )
    except (UnicodeDecodeError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(
            f"{identity_path}: authenticated fixture shutdown response is malformed"
        ) from error
    if not isinstance(payload, dict):
        raise ValueError(f"{identity_path}: fixture shutdown response must be an object")
    require_artifact_value(
        identity_path, "fixture shutdown response schema", payload.get("schema"), HTTP_SHUTDOWN_SCHEMA
    )
    require_artifact_value(
        identity_path, "fixture shutdown response run_id", payload.get("run_id"), document["run_id"]
    )
    require_artifact_value(
        identity_path, "fixture shutdown response PID", payload.get("pid"), identity["pid"]
    )
    require_artifact_value(
        identity_path,
        "fixture shutdown response token SHA-256",
        payload.get("token_sha256"),
        shutdown["token_sha256"],
    )
    while time.monotonic() < deadline:
        current = fixture_server_process_observation(int(identity["pid"]))
        if not fixture_server_identity_matches(identity, current):
            return {
                "ok": True,
                "already_stopped": False,
                "identity": str(identity_path),
                "reason": "authenticated_loopback_shutdown",
            }
        time.sleep(0.02)
    raise ValueError(
        f"{identity_path}: exact fixture server survived authenticated shutdown"
    )


def command_stop_server(args: argparse.Namespace) -> int:
    result = stop_fixture_server(args.identity, args.expected_run_id, args.timeout_secs)
    print(json.dumps(result, sort_keys=True))
    return 0


def command_serve(args: argparse.Namespace) -> int:
    if not args.file.is_file():
        raise ValueError(f"fixture does not exist: {args.file}")
    if not args.run_id:
        raise ValueError("--run-id must not be empty")
    if re.fullmatch(r"[A-Za-z0-9_-]{43,128}", args.shutdown_token) is None:
        raise ValueError("--shutdown-token must be a 43..128 character URL-safe secret")
    for name in ("throttle_bps", "outage_every_bytes", "outage_ms", "disconnect_every_bytes"):
        if getattr(args, name) < 0:
            raise ValueError(f"--{name.replace('_', '-')} must be non-negative")
    started_unix_ns = time.time_ns()
    started_monotonic_ns = time.monotonic_ns()
    server = FixtureServer(
        (args.host, args.port),
        RangeFixtureHandler,
        args.file.resolve(),
        args.throttle_bps,
        args.outage_every_bytes,
        args.outage_ms,
        args.disconnect_every_bytes,
        args.request_log,
        args.run_id,
        args.shutdown_token,
    )
    server.verbose = args.verbose  # type: ignore[attr-defined]
    host, port = server.server_address[:2]
    server_process = fixture_server_process_observation(os.getpid())
    if server_process is None:
        raise ValueError("cannot capture the exact fixture server process identity")
    manifest = {
        "schema": "ytt.tui-perf.http.v1",
        "pid": os.getpid(),
        "host": host,
        "port": port,
        "url": f"http://{host}:{port}/fixture.wav",
        "fixture_sha256": sha256_file(args.file),
        "fixture_bytes": args.file.stat().st_size,
        "run_id": args.run_id,
        "server_process": {
            "schema": "ytt.tui-perf.http-process.v1",
            **server_process,
        },
        "shutdown": {
            "schema": HTTP_SHUTDOWN_SCHEMA,
            "method": HTTP_SHUTDOWN_METHOD,
            "path": HTTP_SHUTDOWN_PATH,
            "run_id": args.run_id,
            "token": args.shutdown_token,
            "token_sha256": hashlib.sha256(
                args.shutdown_token.encode("ascii")
            ).hexdigest(),
        },
        "started_unix_ns": started_unix_ns,
        "started_monotonic_ns": started_monotonic_ns,
        "request_log": str(args.request_log.resolve()),
        "throttle_bps": args.throttle_bps,
        "pacing_policy": "global_monotonic_next_byte_deadline_v1",
        "pacing_origin_monotonic_ns": server.pacing_origin_monotonic_ns,
        "pacing_initial_credit_bytes": 0,
        "pacing_max_chunk_bytes": HTTP_THROTTLE_CHUNK_BYTES,
        "delivery_evidence": "server_socket_bytes_accepted",
        "delivery_evidence_limitation": (
            "wfile_write_flush_proves_kernel_socket_acceptance_not_client_decode_or_playback"
        ),
        "outage_every_bytes": args.outage_every_bytes,
        "outage_ms": args.outage_ms,
        "disconnect_every_bytes": args.disconnect_every_bytes,
        "bind_is_loopback": ipaddress.ip_address(host).is_loopback,
        "playback_target_mode": "local_m3u_indirection",
        "external_dns_required": False,
    }
    if args.ready_file:
        atomic_json(args.ready_file, manifest)
    print(json.dumps(manifest, sort_keys=True), flush=True)
    try:
        server.serve_forever(poll_interval=0.1)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


def validate_http_request_log(
    path: Path,
    http: dict[str, Any],
    profile: dict[str, Any],
    run_contract: dict[str, Any] | None = None,
) -> None:
    records = read_ndjson(path)
    if not records:
        raise ValueError(f"{path}: HTTP request log is empty")
    throttle_bps = non_negative_integer(profile.get("throttle_bps"), "throttle_bps", path)
    outage_every_bytes = non_negative_integer(
        profile.get("outage_every_bytes"), "outage_every_bytes", path
    )
    outage_ms = non_negative_integer(profile.get("outage_ms"), "outage_ms", path)
    disconnect_every_bytes = non_negative_integer(
        profile.get("disconnect_every_bytes"), "disconnect_every_bytes", path
    )
    require_artifact_value(path, "HTTP throttle", http.get("throttle_bps"), throttle_bps)
    require_artifact_value(
        path, "HTTP outage threshold", http.get("outage_every_bytes"), outage_every_bytes
    )
    require_artifact_value(path, "HTTP outage delay", http.get("outage_ms"), outage_ms)
    require_artifact_value(
        path,
        "HTTP disconnect threshold",
        http.get("disconnect_every_bytes"),
        disconnect_every_bytes,
    )
    require_artifact_value(
        path,
        "HTTP pacing policy",
        http.get("pacing_policy"),
        "global_monotonic_next_byte_deadline_v1",
    )
    require_artifact_value(
        path, "HTTP pacing initial credit", http.get("pacing_initial_credit_bytes"), 0
    )
    require_artifact_value(
        path,
        "HTTP pacing maximum chunk",
        http.get("pacing_max_chunk_bytes"),
        HTTP_THROTTLE_CHUNK_BYTES,
    )
    require_artifact_value(
        path,
        "HTTP delivery evidence",
        http.get("delivery_evidence"),
        "server_socket_bytes_accepted",
    )
    require_artifact_value(
        path,
        "HTTP delivery evidence limitation",
        http.get("delivery_evidence_limitation"),
        "wfile_write_flush_proves_kernel_socket_acceptance_not_client_decode_or_playback",
    )
    server_started_ns = non_negative_integer(
        http.get("started_monotonic_ns"), "HTTP server monotonic start", path
    )
    pacing_origin_ns = non_negative_integer(
        http.get("pacing_origin_monotonic_ns"), "HTTP pacing origin", path
    )
    if pacing_origin_ns < server_started_ns:
        raise ValueError(f"{path}: HTTP pacing origin precedes server startup")
    if run_contract is not None and not (
        run_contract["started_monotonic_ns"]
        <= pacing_origin_ns
        <= run_contract["finished_monotonic_ns"]
    ):
        raise ValueError(f"{path}: HTTP pacing origin escapes its run contract")
    starts: dict[int, dict[str, Any]] = {}
    request_delivered: dict[int, int] = {}
    request_last_write_finished: dict[int, int] = {}
    terminal_ids: set[int] = set()
    global_delivered = 0
    get_count = 0
    total_get_server_socket_bytes_accepted = 0
    previous_pacing_deadline_ns = pacing_origin_ns
    previous_write_finished_ns: int | None = None
    fault_thresholds_seen: dict[str, set[int]] = {
        "outage": set(),
        "disconnect": set(),
    }
    pending_outage: dict[str, Any] | None = None
    disconnect_actions: dict[int, int] = {}
    disconnect_terminals: set[int] = set()
    fixture_bytes = non_negative_integer(http.get("fixture_bytes"), "fixture_bytes", path)
    for index, record in enumerate(records):
        require_artifact_value(path, f"HTTP record {index} schema", record.get("schema"), "ytt.tui-perf.http-request.v1")
        require_artifact_value(path, f"HTTP record {index} run_id", record.get("run_id"), http.get("run_id"))
        require_artifact_value(path, f"HTTP record {index} server PID", record.get("server_pid"), http.get("pid"))
        require_artifact_value(path, f"HTTP record {index} fixture", record.get("fixture_sha256"), http.get("fixture_sha256"))
        request_id = non_negative_integer(record.get("request_id"), "request_id", path)
        if request_id == 0:
            raise ValueError(f"{path}: request_id must be positive")
        kind = record.get("kind")
        if kind not in {"request_started", "delivery", "request_terminal"}:
            raise ValueError(f"{path}: HTTP record {index} has unexpected kind {kind!r}")
        method = record.get("method")
        request_path = record.get("path")
        if method not in {"GET", "HEAD"} or request_path != "/fixture.wav":
            raise ValueError(f"{path}: unexpected HTTP request {method!r} {request_path!r}")
        started_ns = non_negative_integer(record.get("started_monotonic_ns"), "HTTP started time", path)
        finished_ns = record.get("finished_monotonic_ns")
        if finished_ns is not None:
            finished_ns = non_negative_integer(finished_ns, "HTTP finished time", path)
            if finished_ns < started_ns:
                raise ValueError(f"{path}: HTTP request finished before it started")
        if run_contract is not None:
            if started_ns < run_contract["started_monotonic_ns"] or (
                finished_ns is not None
                and finished_ns > run_contract["finished_monotonic_ns"]
            ):
                raise ValueError(f"{path}: HTTP request interval escapes its run contract")
        if kind == "request_started":
            if request_id in starts:
                raise ValueError(f"{path}: duplicate HTTP request start {request_id}")
            if finished_ns is not None:
                raise ValueError(f"{path}: HTTP request start {request_id} is already finished")
            status = record.get("status")
            if status not in {200, 206}:
                raise ValueError(f"{path}: started request has invalid status {status!r}")
            planned_start = non_negative_integer(record.get("planned_start"), "planned_start", path)
            planned_end = non_negative_integer(record.get("planned_end"), "planned_end", path)
            planned_bytes = non_negative_integer(record.get("bytes_planned"), "bytes_planned", path)
            if planned_start > planned_end or planned_end >= fixture_bytes or planned_bytes != planned_end - planned_start + 1:
                raise ValueError(f"{path}: request {request_id} has invalid planned byte range")
            if status == 200 and (planned_start != 0 or planned_end != fixture_bytes - 1):
                raise ValueError(f"{path}: full response does not cover the fixture")
            if status == 206 and not isinstance(record.get("range_header"), str):
                raise ValueError(f"{path}: partial response has no Range header")
            require_artifact_value(
                path,
                f"request {request_id} initial server socket bytes accepted",
                record.get("server_socket_bytes_accepted"),
                0,
            )
            starts[request_id] = record
            request_delivered[request_id] = 0
            get_count += int(method == "GET")
            continue
        start = starts.get(request_id)
        if start is None:
            raise ValueError(f"{path}: HTTP {kind} precedes request start {request_id}")
        if request_id in terminal_ids:
            raise ValueError(f"{path}: HTTP {kind} follows terminal record {request_id}")
        for field in ("method", "path", "range_header", "status", "planned_start", "planned_end", "bytes_planned", "started_monotonic_ns"):
            require_artifact_value(path, f"request {request_id} stable {field}", record.get(field), start.get(field))
        if kind == "delivery":
            if request_id in disconnect_actions:
                raise ValueError(
                    f"{path}: delivery follows request {request_id}'s disconnect action"
                )
            if finished_ns is None:
                raise ValueError(f"{path}: HTTP delivery {request_id} has no completion time")
            delivered = non_negative_integer(
                record.get("server_socket_bytes_accepted"),
                "server socket bytes accepted",
                path,
            )
            if delivered == 0:
                raise ValueError(f"{path}: zero-length HTTP delivery")
            if throttle_bps and delivered > HTTP_THROTTLE_CHUNK_BYTES:
                raise ValueError(
                    f"{path}: throttled HTTP delivery exceeds the bounded chunk size"
                )
            request_delivered[request_id] += delivered
            if request_delivered[request_id] > start["bytes_planned"]:
                raise ValueError(f"{path}: request {request_id} delivered beyond its byte range")
            require_artifact_value(
                path,
                f"request {request_id} cumulative server socket bytes accepted",
                record.get("request_server_socket_bytes_accepted"),
                request_delivered[request_id],
            )
            previous_global = global_delivered
            global_delivered += delivered
            require_artifact_value(
                path,
                "global server socket bytes accepted",
                record.get("global_server_socket_bytes_accepted"),
                global_delivered,
            )
            pacing_reserved_ns = non_negative_integer(
                record.get("pacing_reserved_monotonic_ns"),
                "HTTP pacing reservation time",
                path,
            )
            write_started_ns = non_negative_integer(
                record.get("write_started_monotonic_ns"), "HTTP write start", path
            )
            write_finished_ns = non_negative_integer(
                record.get("write_finished_monotonic_ns"), "HTTP write finish", path
            )
            if not (
                start["started_monotonic_ns"]
                <= pacing_reserved_ns
                <= write_started_ns
                <= write_finished_ns
                <= finished_ns
            ):
                raise ValueError(f"{path}: HTTP delivery timestamps are not monotonic")
            if pending_outage is not None:
                required_after_outage_ns = (
                    pending_outage["write_finished_monotonic_ns"]
                    + outage_ms * 1_000_000
                )
                # The transfer lock remains held during an outage.  Consequently both the
                # next global reservation and its write must occur after the delay; use the
                # existing early-wakeup tolerance explicitly when checking that boundary.
                next_reservation_or_write_ns = min(pacing_reserved_ns, write_started_ns)
                if (
                    next_reservation_or_write_ns + HTTP_PACING_EARLY_TOLERANCE_NS
                    < required_after_outage_ns
                ):
                    raise ValueError(
                        f"{path}: outage at threshold "
                        f"{pending_outage['threshold']} has no delayed next global delivery"
                    )
                pending_outage = None
            if (
                previous_write_finished_ns is not None
                and write_started_ns < previous_write_finished_ns
            ):
                raise ValueError(f"{path}: globally serialized HTTP writes overlap")
            if throttle_bps:
                pacing_deadline_ns = non_negative_integer(
                    record.get("pacing_deadline_monotonic_ns"),
                    "HTTP pacing deadline",
                    path,
                )
                byte_time_ns = (
                    delivered * 1_000_000_000 + throttle_bps - 1
                ) // throttle_bps
                expected_deadline_ns = max(
                    previous_pacing_deadline_ns, pacing_reserved_ns
                ) + byte_time_ns
                require_artifact_value(
                    path,
                    "global HTTP pacing deadline",
                    pacing_deadline_ns,
                    expected_deadline_ns,
                )
                if write_started_ns + HTTP_PACING_EARLY_TOLERANCE_NS < pacing_deadline_ns:
                    raise ValueError(f"{path}: HTTP bytes were written before their pacing deadline")
                global_envelope_ns = pacing_origin_ns + (
                    global_delivered * 1_000_000_000 + throttle_bps - 1
                ) // throttle_bps
                if write_started_ns + HTTP_PACING_EARLY_TOLERANCE_NS < global_envelope_ns:
                    raise ValueError(f"{path}: global HTTP byte-time envelope was exceeded")
                previous_pacing_deadline_ns = pacing_deadline_ns
            elif record.get("pacing_deadline_monotonic_ns") is not None:
                raise ValueError(f"{path}: unthrottled delivery recorded a pacing deadline")
            previous_write_finished_ns = write_finished_ns
            request_last_write_finished[request_id] = write_finished_ns
            expected_actions = []
            for action, every in (
                ("outage", outage_every_bytes),
                ("disconnect", disconnect_every_bytes),
            ):
                if every:
                    first = ((previous_global // every) + 1) * every
                    expected_actions.extend(
                        {"action": action, "threshold": threshold}
                        for threshold in range(first, global_delivered + 1, every)
                    )
            expected_actions.sort(key=lambda item: (item["threshold"], 0 if item["action"] == "outage" else 1))
            actual_actions = record.get("outage_or_disconnect_action") or []
            if not isinstance(actual_actions, list):
                raise ValueError(f"{path}: delivery fault action must be an array or null")
            if not all(isinstance(action, dict) for action in actual_actions):
                raise ValueError(f"{path}: delivery fault actions must be objects")
            actual_actions = sorted(
                actual_actions,
                key=lambda item: (
                    item.get("threshold", -1),
                    0 if item.get("action") == "outage" else 1,
                ),
            )
            require_artifact_value(path, "HTTP fault action sequence", actual_actions, expected_actions)
            action_names = {action["action"] for action in actual_actions}
            for action in actual_actions:
                fault_thresholds_seen[action["action"]].add(action["threshold"])
            outage_actions = [
                action for action in actual_actions if action["action"] == "outage"
            ]
            if outage_actions:
                pending_outage = {
                    "request_id": request_id,
                    "threshold": outage_actions[-1]["threshold"],
                    "write_finished_monotonic_ns": write_finished_ns,
                    "coincident_disconnect": "disconnect" in action_names,
                }
            disconnect_actions_for_delivery = [
                action for action in actual_actions if action["action"] == "disconnect"
            ]
            if disconnect_actions_for_delivery:
                if request_id in disconnect_actions:
                    raise ValueError(
                        f"{path}: request {request_id} has multiple disconnect actions"
                    )
                disconnect_actions[request_id] = disconnect_actions_for_delivery[-1][
                    "threshold"
                ]
            if start["method"] == "GET":
                total_get_server_socket_bytes_accepted += delivered
        else:
            if finished_ns is None:
                raise ValueError(f"{path}: HTTP terminal record {request_id} has no finish time")
            terminal_ids.add(request_id)
            require_artifact_value(
                path,
                f"request {request_id} terminal server socket bytes accepted",
                record.get("server_socket_bytes_accepted"),
                request_delivered[request_id],
            )
            if not isinstance(record.get("terminal_reason"), str) or not record["terminal_reason"]:
                raise ValueError(f"{path}: HTTP terminal reason is missing")
            disconnect_threshold = disconnect_actions.get(request_id)
            if disconnect_threshold is not None:
                if record["terminal_reason"] != "planned_disconnect":
                    raise ValueError(
                        f"{path}: disconnect action at threshold {disconnect_threshold} "
                        "is not bound to a planned_disconnect terminal"
                    )
                disconnect_terminals.add(request_id)
            elif record["terminal_reason"] == "planned_disconnect":
                raise ValueError(
                    f"{path}: planned_disconnect terminal {request_id} has no disconnect action"
                )
            if finished_ns < request_last_write_finished.get(request_id, start["started_monotonic_ns"]):
                raise ValueError(f"{path}: HTTP terminal record predates its final write")
            if (
                pending_outage is not None
                and pending_outage["request_id"] == request_id
                and pending_outage["coincident_disconnect"]
                and record["terminal_reason"] == "planned_disconnect"
            ):
                required_after_outage_ns = (
                    pending_outage["write_finished_monotonic_ns"]
                    + outage_ms * 1_000_000
                )
                if finished_ns + HTTP_PACING_EARLY_TOLERANCE_NS < required_after_outage_ns:
                    raise ValueError(
                        f"{path}: outage at threshold "
                        f"{pending_outage['threshold']} has no delayed disconnect terminal"
                    )
                pending_outage = None
            if record["terminal_reason"] == "complete" and (
                start["method"] != "GET"
                or request_delivered[request_id] != start["bytes_planned"]
            ):
                raise ValueError(f"{path}: completed HTTP request did not deliver its full range")
            if record["terminal_reason"] == "head_complete" and (
                start["method"] != "HEAD" or request_delivered[request_id] != 0
            ):
                raise ValueError(f"{path}: HEAD completion contains body bytes")
    for action, first_threshold in (
        ("outage", outage_every_bytes),
        ("disconnect", disconnect_every_bytes),
    ):
        if first_threshold and first_threshold not in fault_thresholds_seen[action]:
            raise ValueError(
                f"{path}: configured {action} fault did not cross and record its first "
                f"threshold {first_threshold}"
            )
    missing_disconnect_terminals = set(disconnect_actions) - disconnect_terminals
    if missing_disconnect_terminals:
        raise ValueError(
            f"{path}: disconnect actions have no planned_disconnect terminal for requests "
            f"{sorted(missing_disconnect_terminals)}"
        )
    if pending_outage is not None:
        raise ValueError(
            f"{path}: outage at threshold {pending_outage['threshold']} has no delayed "
            "next global delivery"
        )
    # The orchestrator terminates the per-run fixture process after the measured owner and mpv
    # are gone.  A handler that was blocked in a paced write may therefore have no terminal
    # record.  Disconnect actions are the exception because the handler emits their bound
    # terminal immediately; outage actions also need the timing proof checked above.
    if fixture_bytes < HTTP_MEANINGFUL_GET_BYTES:
        raise ValueError(f"{path}: fixture is too small to prove meaningful playback")
    if (
        get_count < 1
        or total_get_server_socket_bytes_accepted < HTTP_MEANINGFUL_GET_BYTES
    ):
        raise ValueError(
            f"{path}: server did not prove at least {HTTP_MEANINGFUL_GET_BYTES} "
            "GET bytes accepted by the server socket "
            f"(GETs={get_count}, bytes={total_get_server_socket_bytes_accepted}); "
            "this is transport evidence, not client decode or playback proof"
        )


def http_pacing_self_test() -> None:
    throttle_bps = 64 * 1024
    fixture_bytes = HTTP_MEANINGFUL_GET_BYTES
    origin_ns = 10_000_000_000
    profile = {
        "throttle_bps": throttle_bps,
        "outage_every_bytes": 0,
        "outage_ms": 0,
        "disconnect_every_bytes": 0,
    }
    http = {
        "schema": "ytt.tui-perf.http.v1",
        "pid": 4242,
        "run_id": "http-self-test",
        "fixture_sha256": "a" * 64,
        "fixture_bytes": fixture_bytes,
        "started_monotonic_ns": origin_ns - 1,
        "throttle_bps": throttle_bps,
        "pacing_policy": "global_monotonic_next_byte_deadline_v1",
        "pacing_origin_monotonic_ns": origin_ns,
        "pacing_initial_credit_bytes": 0,
        "pacing_max_chunk_bytes": HTTP_THROTTLE_CHUNK_BYTES,
        "delivery_evidence": "server_socket_bytes_accepted",
        "delivery_evidence_limitation": (
            "wfile_write_flush_proves_kernel_socket_acceptance_not_client_decode_or_playback"
        ),
        "outage_every_bytes": profile["outage_every_bytes"],
        "outage_ms": profile["outage_ms"],
        "disconnect_every_bytes": profile["disconnect_every_bytes"],
    }

    def build_records(
        requests: list[dict[str, Any]],
        schedule: list[tuple[int, int]],
        traffic_profile: dict[str, Any] | None = None,
        apply_outage_delay: bool = True,
    ) -> list[dict[str, Any]]:
        active_profile = traffic_profile or profile
        active_throttle_bps = int(active_profile["throttle_bps"])
        records: list[dict[str, Any]] = []
        specs = {int(item["request_id"]): item for item in requests}
        accepted = {request_id: 0 for request_id in specs}
        disconnected: set[int] = set()

        def common() -> dict[str, Any]:
            return {
                "schema": "ytt.tui-perf.http-request.v1",
                "run_id": http["run_id"],
                "server_pid": http["pid"],
                "fixture_sha256": http["fixture_sha256"],
            }

        for request_id, spec in specs.items():
            planned = int(spec["planned_end"]) - int(spec["planned_start"]) + 1
            records.append(
                {
                    **common(),
                    "kind": "request_started",
                    "request_id": request_id,
                    "method": "GET",
                    "path": "/fixture.wav",
                    "range_header": spec["range_header"],
                    "status": spec["status"],
                    "planned_start": spec["planned_start"],
                    "planned_end": spec["planned_end"],
                    "bytes_planned": planned,
                    "server_socket_bytes_accepted": 0,
                    "started_monotonic_ns": origin_ns,
                    "finished_monotonic_ns": None,
                    "terminal_reason": None,
                    "outage_or_disconnect_action": None,
                }
            )

        previous_deadline_ns = origin_ns
        previous_write_finished_ns = origin_ns
        next_transfer_available_ns = origin_ns
        global_accepted = 0
        for request_id, byte_count in schedule:
            if request_id in disconnected:
                raise AssertionError("synthetic HTTP schedule continued after disconnect")
            spec = specs[request_id]
            planned = int(spec["planned_end"]) - int(spec["planned_start"]) + 1
            reserved_ns = next_transfer_available_ns
            if active_throttle_bps:
                byte_time_ns = (
                    byte_count * 1_000_000_000 + active_throttle_bps - 1
                ) // active_throttle_bps
                deadline_ns = max(previous_deadline_ns, reserved_ns) + byte_time_ns
                write_started_ns = deadline_ns
            else:
                deadline_ns = None
                write_started_ns = reserved_ns
            write_finished_ns = write_started_ns + 1
            accepted[request_id] += byte_count
            previous_global = global_accepted
            global_accepted += byte_count
            actions: list[dict[str, Any]] = []
            for action, every in (
                ("outage", int(active_profile["outage_every_bytes"])),
                ("disconnect", int(active_profile["disconnect_every_bytes"])),
            ):
                if every:
                    first = ((previous_global // every) + 1) * every
                    actions.extend(
                        {"action": action, "threshold": threshold}
                        for threshold in range(first, global_accepted + 1, every)
                    )
            actions.sort(
                key=lambda item: (
                    item["threshold"],
                    0 if item["action"] == "outage" else 1,
                )
            )
            records.append(
                {
                    **common(),
                    "kind": "delivery",
                    "request_id": request_id,
                    "method": "GET",
                    "path": "/fixture.wav",
                    "range_header": spec["range_header"],
                    "status": spec["status"],
                    "planned_start": spec["planned_start"],
                    "planned_end": spec["planned_end"],
                    "bytes_planned": planned,
                    "server_socket_bytes_accepted": byte_count,
                    "request_server_socket_bytes_accepted": accepted[request_id],
                    "global_server_socket_bytes_accepted": global_accepted,
                    "started_monotonic_ns": origin_ns,
                    "finished_monotonic_ns": write_finished_ns,
                    "terminal_reason": None,
                    "pacing_reserved_monotonic_ns": reserved_ns,
                    "pacing_deadline_monotonic_ns": deadline_ns,
                    "write_started_monotonic_ns": write_started_ns,
                    "write_finished_monotonic_ns": write_finished_ns,
                    "outage_or_disconnect_action": actions or None,
                }
            )
            if deadline_ns is not None:
                previous_deadline_ns = deadline_ns
            previous_write_finished_ns = write_finished_ns
            next_transfer_available_ns = write_finished_ns
            if apply_outage_delay and any(
                action["action"] == "outage" for action in actions
            ):
                next_transfer_available_ns += int(active_profile["outage_ms"]) * 1_000_000
            if any(action["action"] == "disconnect" for action in actions):
                disconnected.add(request_id)

        for request_id, spec in specs.items():
            planned = int(spec["planned_end"]) - int(spec["planned_start"]) + 1
            if accepted[request_id] != planned and request_id not in disconnected:
                continue
            records.append(
                {
                    **common(),
                    "kind": "request_terminal",
                    "request_id": request_id,
                    "method": "GET",
                    "path": "/fixture.wav",
                    "range_header": spec["range_header"],
                    "status": spec["status"],
                    "planned_start": spec["planned_start"],
                    "planned_end": spec["planned_end"],
                    "bytes_planned": planned,
                    "server_socket_bytes_accepted": accepted[request_id],
                    "started_monotonic_ns": origin_ns,
                    "finished_monotonic_ns": (
                        max(previous_write_finished_ns, next_transfer_available_ns)
                        + request_id
                    ),
                    "terminal_reason": (
                        "planned_disconnect"
                        if request_id in disconnected
                        else "complete"
                    ),
                    "outage_or_disconnect_action": None,
                }
            )
        return records

    full_request = [
        {
            "request_id": 1,
            "range_header": None,
            "status": 200,
            "planned_start": 0,
            "planned_end": fixture_bytes - 1,
        }
    ]
    split_requests = [
        {
            "request_id": 1,
            "range_header": "bytes=0-32767",
            "status": 206,
            "planned_start": 0,
            "planned_end": fixture_bytes // 2 - 1,
        },
        {
            "request_id": 2,
            "range_header": "bytes=32768-65535",
            "status": 206,
            "planned_start": fixture_bytes // 2,
            "planned_end": fixture_bytes - 1,
        },
    ]

    with tempfile.TemporaryDirectory(prefix="ytt-perf-http-self-test-") as temporary:
        root = Path(temporary)

        def write_log(name: str, records: list[dict[str, Any]]) -> Path:
            path = root / f"{name}.ndjson"
            path.write_text(
                "".join(
                    json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
                    for record in records
                ),
                encoding="utf-8",
            )
            return path

        def expect_rejected(
            name: str,
            records: list[dict[str, Any]],
            http_document: dict[str, Any] | None = None,
            traffic_profile: dict[str, Any] | None = None,
        ) -> None:
            try:
                validate_http_request_log(
                    write_log(name, records),
                    http_document or http,
                    traffic_profile or profile,
                )
            except ValueError:
                pass
            else:
                raise AssertionError(f"HTTP validator accepted hostile proof {name}")

        def http_for_profile(traffic_profile: dict[str, Any]) -> dict[str, Any]:
            return {
                **http,
                "throttle_bps": traffic_profile["throttle_bps"],
                "outage_every_bytes": traffic_profile["outage_every_bytes"],
                "outage_ms": traffic_profile["outage_ms"],
                "disconnect_every_bytes": traffic_profile["disconnect_every_bytes"],
            }

        valid = build_records(
            split_requests,
            [
                (request_id, HTTP_THROTTLE_CHUNK_BYTES)
                for _round in range(8)
                for request_id in (1, 2)
            ],
        )
        validate_http_request_log(write_log("valid-shared-global-budget", valid), http, profile)

        instant = json.loads(json.dumps(valid))
        first_delivery = next(
            record for record in instant if record["kind"] == "delivery"
        )
        first_delivery["write_started_monotonic_ns"] = (
            first_delivery["pacing_deadline_monotonic_ns"]
            - HTTP_PACING_EARLY_TOLERANCE_NS
            - 1
        )
        expect_rejected("instant-64k", instant)

        large_chunk = build_records(full_request, [(1, fixture_bytes)])
        expect_rejected("single-64k-chunk", large_chunk)

        tiny_only = build_records(full_request, [(1, HTTP_THROTTLE_CHUNK_BYTES)])
        expect_rejected("tiny-only", tiny_only)

        tampered_credit = {**http, "pacing_initial_credit_bytes": fixture_bytes}
        expect_rejected("initial-credit", valid, tampered_credit)

        unexercised_fault_profile = {
            "throttle_bps": throttle_bps,
            "outage_every_bytes": fixture_bytes * 2,
            "outage_ms": 1_500,
            "disconnect_every_bytes": fixture_bytes * 4,
        }
        expect_rejected(
            "configured-faults-unexercised-64k",
            valid,
            http_for_profile(unexercised_fault_profile),
            unexercised_fault_profile,
        )

        exercised_fault_profile = {
            "throttle_bps": throttle_bps,
            "outage_every_bytes": fixture_bytes // 2,
            "outage_ms": 1_500,
            "disconnect_every_bytes": fixture_bytes,
        }
        fault_schedule = [
            (1, HTTP_THROTTLE_CHUNK_BYTES)
            for _ in range(fixture_bytes // HTTP_THROTTLE_CHUNK_BYTES)
        ]
        missing_outage_delay = build_records(
            full_request,
            fault_schedule,
            exercised_fault_profile,
            apply_outage_delay=False,
        )
        expect_rejected(
            "correctly-marked-missing-outage-delay",
            missing_outage_delay,
            http_for_profile(exercised_fault_profile),
            exercised_fault_profile,
        )
        delayed_outage_and_disconnect = build_records(
            full_request,
            fault_schedule,
            exercised_fault_profile,
        )
        validate_http_request_log(
            write_log(
                "delayed-outage-and-planned-disconnect",
                delayed_outage_and_disconnect,
            ),
            http_for_profile(exercised_fault_profile),
            exercised_fault_profile,
        )

        fixture = root / "fixture.bin"
        fixture.write_bytes(b"\0" * fixture_bytes)
        server = FixtureServer(
            ("127.0.0.1", 0),
            RangeFixtureHandler,
            fixture,
            throttle_bps,
            0,
            0,
            0,
            root / "reservation.ndjson",
            "reservation-self-test",
        )
        try:
            first_reserved, first_deadline = server.reserve_pacing_deadline(
                HTTP_THROTTLE_CHUNK_BYTES
            )
            byte_time_ns = (
                HTTP_THROTTLE_CHUNK_BYTES * 1_000_000_000 + throttle_bps - 1
            ) // throttle_bps
            if first_deadline != (
                max(server.pacing_origin_monotonic_ns, first_reserved) + byte_time_ns
            ):
                raise AssertionError("HTTP throttle granted initial byte credit")
            second_reserved, second_deadline = server.reserve_pacing_deadline(
                HTTP_THROTTLE_CHUNK_BYTES
            )
            if second_deadline != max(first_deadline, second_reserved) + byte_time_ns:
                raise AssertionError("HTTP throttle did not share its global pacing deadline")
        finally:
            server.server_close()

        import http.client as http_client

        live_log = root / "live-handler.ndjson"
        live_server = FixtureServer(
            ("127.0.0.1", 0),
            RangeFixtureHandler,
            fixture,
            throttle_bps,
            0,
            0,
            0,
            live_log,
            "live-handler-self-test",
        )
        live_thread = threading.Thread(
            target=live_server.serve_forever,
            kwargs={"poll_interval": 0.01},
            daemon=True,
        )
        live_thread.start()
        connection = http_client.HTTPConnection(
            str(live_server.server_address[0]),
            int(live_server.server_address[1]),
            timeout=5,
        )
        try:
            connection.request("GET", "/fixture.wav")
            response = connection.getresponse()
            body = response.read()
            if response.status != 200 or len(body) != fixture_bytes:
                raise AssertionError("live HTTP fixture did not return the complete fixture")
        finally:
            connection.close()
            live_server.shutdown()
            live_thread.join(timeout=5)
            live_server.server_close()
        if live_thread.is_alive():
            raise AssertionError("live HTTP fixture self-test did not stop")
        validate_http_request_log(
            live_log,
            {
                **http,
                "pid": os.getpid(),
                "run_id": "live-handler-self-test",
                "fixture_sha256": sha256_file(fixture),
                "started_monotonic_ns": live_server.pacing_origin_monotonic_ns - 1,
                "pacing_origin_monotonic_ns": live_server.pacing_origin_monotonic_ns,
            },
            profile,
        )


def http_server_shutdown_self_test() -> None:
    if os.name == "nt":
        return
    with tempfile.TemporaryDirectory(
        prefix="ytt-perf-http-shutdown-self-test-"
    ) as temporary:
        root = Path(temporary)
        fixture = root / "fixture.bin"
        request_log = root / "requests.ndjson"
        ready = root / "ready.json"
        fixture.write_bytes(b"fixture-server-shutdown-self-test")
        run_id = "http-shutdown-self-test"
        # Force the argparse-hostile edge case: a separate value beginning with `-` can be
        # mistaken for another option, so every real launch must use --shutdown-token=VALUE.
        shutdown_token = "-" + "A" * 42
        process = subprocess.Popen(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "serve",
                "--file",
                str(fixture),
                "--ready-file",
                str(ready),
                "--request-log",
                str(request_log),
                "--run-id",
                run_id,
                f"--shutdown-token={shutdown_token}",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        sentinel: subprocess.Popen[str] | None = None
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not ready.is_file():
                if process.poll() is not None:
                    raise AssertionError(
                        "fixture shutdown self-test server exited before readiness: "
                        f"{process.stderr.read()}"
                    )
                time.sleep(0.01)
            if not ready.is_file():
                raise AssertionError("fixture shutdown self-test server did not become ready")
            manifest = load_json_object(ready)
            require_artifact_value(ready, "self-test server PID", manifest.get("pid"), process.pid)
            stopped = stop_fixture_server(ready, run_id, 5.0)
            if stopped["already_stopped"]:
                raise AssertionError("live fixture server was reported as already stopped")
            process.wait(timeout=2)
            if process.returncode != 0:
                raise AssertionError(
                    "fixture server failed during authenticated shutdown: "
                    f"{process.stderr.read()}"
                )

            sentinel = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "import time; time.sleep(60)",
                    "serve",
                    "--run-id",
                    run_id,
                    f"--shutdown-token={shutdown_token}",
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            sentinel_identity: dict[str, Any] | None = None
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and sentinel_identity is None:
                sentinel_identity = fixture_server_process_observation(sentinel.pid)
                time.sleep(0.01)
            if sentinel_identity is None:
                raise AssertionError("stale-PID sentinel did not become observable")
            stale_token = manifest["server_process"]["native_start_token"]
            if sentinel_identity["native_start_token"] == stale_token:
                raise AssertionError("stale-PID fixture did not receive a distinct start token")
            sentinel_identity["native_start_token"] = stale_token
            stale_manifest = json.loads(json.dumps(manifest))
            stale_manifest["pid"] = sentinel.pid
            stale_manifest["server_process"] = {
                "schema": "ytt.tui-perf.http-process.v1",
                **sentinel_identity,
            }
            stale_path = root / "stale-ready.json"
            atomic_json(stale_path, stale_manifest)
            stale_result = stop_fixture_server(stale_path, run_id, 0.5)
            if not stale_result["already_stopped"]:
                raise AssertionError("stale fixture server identity was treated as live")
            if sentinel.poll() is not None:
                raise AssertionError(
                    "stale-PID fixture server shutdown signaled the unrelated sentinel"
                )
        finally:
            for child in (sentinel, process):
                if child is not None and child.poll() is None:
                    child.kill()
                if child is not None:
                    with contextlib.suppress(subprocess.TimeoutExpired):
                        child.wait(timeout=2)


def read_ndjson(path: Path) -> list[dict[str, Any]]:
    records = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            record = json.loads(line, object_pairs_hook=reject_duplicate_json_keys)
        except (json.JSONDecodeError, DuplicateJsonKeyError) as error:
            raise ValueError(f"{path}:{number}: malformed NDJSON: {error}") from error
        if not isinstance(record, dict):
            raise ValueError(f"{path}:{number}: NDJSON record is not an object")
        records.append(record)
    return records


def quantile(values: list[float], q: float) -> float:
    if not values:
        raise ValueError("cannot take a quantile of an empty list")
    ordered = sorted(values)
    index = math.ceil((len(ordered) - 1) * q)
    return ordered[min(index, len(ordered) - 1)]


def exact_latency_histogram(
    value: Any, path: Path, label: str
) -> list[tuple[int, int]]:
    if not isinstance(value, list) or not value:
        raise ValueError(f"{path}: {label} latency_histogram must be non-empty")
    histogram = []
    previous_ns = -1
    for index, bucket in enumerate(value):
        if not isinstance(bucket, dict) or set(bucket) != {"ns", "count"}:
            raise ValueError(f"{path}: {label} histogram bucket {index} has invalid schema")
        ns = non_negative_integer(bucket.get("ns"), f"{label} histogram ns", path)
        count = non_negative_integer(bucket.get("count"), f"{label} histogram count", path)
        if count == 0 or ns <= previous_ns:
            raise ValueError(f"{path}: {label} histogram must be strictly sorted with positive counts")
        previous_ns = ns
        histogram.append((ns, count))
    return histogram


def histogram_quantile(histogram: list[tuple[int, int]], q: float) -> int:
    count = sum(bucket_count for _ns, bucket_count in histogram)
    target = math.ceil((count - 1) * q)
    seen = 0
    for ns, bucket_count in histogram:
        seen += bucket_count
        if target < seen:
            return ns
    raise AssertionError("non-empty histogram did not contain its quantile")


def merged_histogram(histograms: list[list[tuple[int, int]]]) -> list[tuple[int, int]]:
    merged: dict[int, int] = {}
    for histogram in histograms:
        for ns, count in histogram:
            merged[ns] = merged.get(ns, 0) + count
    return sorted(merged.items())


def render_metrics_from_document(document: dict[str, Any], path: Path) -> dict[str, Any]:
    metrics: dict[str, Any] = {}
    for case in document.get("cases", []):
        batches = case.get("batches", [])
        if not batches:
            raise ValueError(f"{path}: render case {case.get('name')} has no batches")
        batch_draws = [batch.get("draws") for batch in batches]
        if any(not isinstance(draws, int) or isinstance(draws, bool) or draws <= 0 for draws in batch_draws):
            raise ValueError(f"{path}: render case {case.get('name')} has invalid batch draws")
        measured_draws = case.get("measured_draws")
        if measured_draws != sum(batch_draws):
            raise ValueError(
                f"{path}: render case {case.get('name')} measured_draws {measured_draws!r} "
                f"does not match batch total {sum(batch_draws)}"
            )
        batch_histograms = []
        for batch_index, batch in enumerate(batches):
            label = f"render case {case.get('name')} batch {batch_index}"
            histogram = exact_latency_histogram(batch.get("latency_histogram"), path, label)
            count = sum(bucket_count for _ns, bucket_count in histogram)
            total = sum(ns * bucket_count for ns, bucket_count in histogram)
            require_artifact_value(path, f"{label} histogram count", count, batch["draws"])
            require_artifact_value(path, f"{label} total", batch.get("total_ns"), total)
            mean = finite_non_negative_number(batch.get("mean_draw_ns"), f"{label} mean", path)
            if not math.isclose(mean, total / count, rel_tol=1e-12, abs_tol=1e-9):
                raise ValueError(f"{path}: {label} mean does not match histogram")
            require_artifact_value(path, f"{label} p50", batch.get("p50_draw_ns"), histogram_quantile(histogram, 0.50))
            require_artifact_value(path, f"{label} p95", batch.get("p95_draw_ns"), histogram_quantile(histogram, 0.95))
            require_artifact_value(path, f"{label} max", batch.get("max_draw_ns"), histogram[-1][0])
            allocation_values = {
                allocation_field: non_negative_integer(
                    batch.get(allocation_field), f"{label} {allocation_field}", path
                )
                for allocation_field in (
                    "allocations", "reallocations", "allocated_bytes", "deallocated_bytes",
                    "peak_live_bytes_delta",
                )
            }
            retained_bytes_delta = batch.get("retained_bytes_delta")
            if not isinstance(retained_bytes_delta, int) or isinstance(retained_bytes_delta, bool):
                raise ValueError(f"{path}: {label} retained_bytes_delta must be an integer")
            expected_retained = (
                allocation_values["allocated_bytes"]
                - allocation_values["deallocated_bytes"]
            )
            require_artifact_value(
                path,
                f"{label} allocator byte conservation",
                retained_bytes_delta,
                expected_retained,
            )
            if allocation_values["peak_live_bytes_delta"] < max(0, retained_bytes_delta):
                raise ValueError(
                    f"{path}: {label} peak live bytes cannot be below final retained bytes"
                )
            batch_histograms.append(histogram)

        case_histogram = exact_latency_histogram(
            case.get("latency_histogram"), path, f"render case {case.get('name')}"
        )
        expected_case_histogram = merged_histogram(batch_histograms)
        require_artifact_value(path, f"render case {case.get('name')} global histogram", case_histogram, expected_case_histogram)
        case_count = sum(count for _ns, count in case_histogram)
        case_total = sum(ns * count for ns, count in case_histogram)
        require_artifact_value(path, f"render case {case.get('name')} total", case.get("total_draw_ns"), case_total)
        case_mean = finite_non_negative_number(case.get("mean_draw_ns"), f"render case {case.get('name')} mean", path)
        if not math.isclose(case_mean, case_total / case_count, rel_tol=1e-12, abs_tol=1e-9):
            raise ValueError(f"{path}: render case {case.get('name')} global mean does not match histogram")
        require_artifact_value(path, f"render case {case.get('name')} p50", case.get("p50_draw_ns"), histogram_quantile(case_histogram, 0.50))
        case_p95 = histogram_quantile(case_histogram, 0.95)
        require_artifact_value(path, f"render case {case.get('name')} p95", case.get("p95_draw_ns"), case_p95)
        require_artifact_value(path, f"render case {case.get('name')} max", case.get("max_draw_ns"), case_histogram[-1][0])

        prefix = f"render.{case['name']}"
        for name in (
            "mean_draw_ns",
            "allocations",
            "allocated_bytes",
            "retained_bytes_delta",
            "peak_live_bytes_delta",
        ):
            values = [float(batch[name]) for batch in batches]
            value = statistics.fmean(values)
            if name in {"allocations", "allocated_bytes"}:
                value /= float(batches[0]["draws"])
                name += "_per_draw"
            metrics[f"{prefix}.{name}"] = value
        metrics[f"{prefix}.p95_draw_ns"] = float(case_p95)
        metrics[f"{prefix}.buffer_style_digest"] = case["buffer_style_digest"]
        metrics[f"{prefix}.hit_map_digest"] = case["hit_map_digest"]
        metrics[f"{prefix}.update_path"] = case["update_path"]
        if case["update_path"] == "app_update_msg_key":
            metrics[f"{prefix}.p95_reducer_input_to_draw_ns"] = float(case_p95)
    return metrics


def metrics_from_file(path: Path) -> dict[str, Any]:
    metrics: dict[str, Any] = {}
    if path.suffix == ".ndjson":
        records = read_ndjson(path)
        errors = [record for record in records if record.get("kind") == "error"]
        if errors:
            raise ValueError(f"{path}: harness error: {errors[-1].get('message')}")
        for record in records:
            if record.get("kind") == "summary" and isinstance(record.get("roles"), dict):
                for role, values in record["roles"].items():
                    if isinstance(values, dict):
                        for name, value in values.items():
                            metrics[f"{role}.{name}"] = value
            if record.get("kind") == "summary" and "buffering_events" in record:
                metrics["buffering_events"] = record["buffering_events"]
                metrics["buffering_ms"] = record["buffering_ms"]
        measured_samples = [
            record
            for record in records
            if record.get("kind") == "sample" and record.get("phase") == "measure"
        ]
        role_names = {
            role
            for record in measured_samples
            for role in record.get("roles", {})
            if isinstance(record.get("roles", {}).get(role), dict)
        }
        for role in role_names:
            points = [
                (float(record["elapsed_ms"]) / 1_000.0, float(record["roles"][role]["rss_bytes"]))
                for record in measured_samples
                if role in record.get("roles", {})
            ]
            if len(points) >= 2:
                metrics[f"{role}.rss_slope_bytes_per_s"] = linear_slope(points)
        operations: dict[str, list[float]] = {}
        for record in records:
            if record.get("kind") == "operation":
                started_ns = record.get("operation_started_ns")
                completed_ns = record.get("operation_completed_ns")
                if (
                    not isinstance(started_ns, int)
                    or isinstance(started_ns, bool)
                    or not isinstance(completed_ns, int)
                    or isinstance(completed_ns, bool)
                    or completed_ns < started_ns
                ):
                    raise ValueError(f"{path}: operation has invalid raw timestamps")
                operations.setdefault(str(record.get("operation")), []).append(
                    (completed_ns - started_ns) / 1_000_000.0
                )
        for name, values in operations.items():
            metrics[f"operation.{name}.p95_ms"] = quantile(values, 0.95)
            metrics[f"operation.{name}.mean_ms"] = statistics.fmean(values)
        return metrics

    document = json.loads(
        path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicate_json_keys
    )
    if document.get("schema") == "ytt.tui-perf.render.v1":
        return render_metrics_from_document(document, path)
    return metrics


def load_metric_files(path: Path) -> dict[str, Any]:
    files = [path] if path.is_file() else sorted(
        item
        for item in path.iterdir()
        if item.is_file() and item.suffix in {".json", ".ndjson"}
    )
    metrics: dict[str, Any] = {}
    for file in files:
        for name, value in metrics_from_file(file).items():
            if name in metrics and metrics[name] != value:
                raise ValueError(f"{path}: duplicate metric {name!r} disagrees across files")
            metrics[name] = value
    return metrics


def load_run(path: Path) -> dict[str, Any]:
    if not path.exists():
        raise ValueError(f"run path does not exist: {path}")
    metrics = load_metric_files(path)
    if path.is_dir():
        geometry_dirs = sorted(
            item
            for item in path.iterdir()
            if item.is_dir() and item.name.startswith("geometry-")
        )
        for directory in geometry_dirs:
            geometry = directory.name.removeprefix("geometry-")
            nested = load_metric_files(directory)
            if not nested:
                raise ValueError(f"{directory}: no recognized performance metrics")
            for name, value in nested.items():
                qualified = f"geometry.{geometry}.{name}"
                if qualified in metrics:
                    raise ValueError(f"{path}: duplicate metric {qualified!r}")
                metrics[qualified] = value
    if not metrics:
        raise ValueError(f"{path}: no recognized performance metrics")
    return metrics


def load_json_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicate_json_keys
        )
    except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(f"cannot read JSON artifact {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"JSON artifact is not an object: {path}")
    return value


def normalized_os(value: Any) -> str:
    name = str(value).strip().lower()
    aliases = {
        "darwin": "macos",
        "macos": "macos",
        "linux": "linux",
        "windows": "windows",
    }
    return aliases.get(name, name)


def require_artifact_value(path: Path, label: str, actual: Any, expected: Any) -> None:
    if actual != expected:
        raise ValueError(f"{path}: {label} is {actual!r}, expected {expected!r}")


def validate_host_manifest(
    path: Path,
    scenario: dict[str, Any],
    scenario_document: dict[str, Any],
    scenario_hash: str,
    render: bool,
) -> dict[str, Any]:
    manifest = load_json_object(path)
    require_artifact_value(path, "schema", manifest.get("schema"), "ytt.tui-perf.host.v1")
    require_artifact_value(path, "scenario", manifest.get("scenario"), scenario["id"])
    require_artifact_value(path, "scenario_sha256", manifest.get("scenario_sha256"), scenario_hash)
    require_artifact_value(
        path, "measurement scope", manifest.get("measurement_scope"), scenario_document["sampling"]
    )
    require_artifact_value(
        path,
        "sampling and cleanup limitations",
        manifest.get("limitations"),
        measurement_limitations(render),
    )
    evidence_root = path.resolve().parent
    scenario_identity = manifest.get("scenario_file")
    if not isinstance(scenario_identity, dict):
        raise ValueError(f"{path}: scenario_file must be an object")
    scenario_path = (evidence_root / str(scenario_identity.get("path", ""))).resolve()
    try:
        scenario_path.relative_to(evidence_root)
    except ValueError as error:
        raise ValueError(f"{path}: scenario snapshot escapes evidence root") from error
    current_scenario = identity_for_file(scenario_path)
    for field in ("bytes", "sha256"):
        require_artifact_value(path, f"scenario snapshot {field}", scenario_identity.get(field), current_scenario[field])
    require_artifact_value(path, "scenario snapshot SHA-256", current_scenario["sha256"], scenario_hash)
    host = manifest.get("host")
    if not isinstance(host, dict):
        raise ValueError(f"{path}: host must be an object")
    current_host_identity = stable_host_identity()
    for field, expected in current_host_identity.items():
        require_artifact_value(path, f"host {field}", host.get(field), expected)

    expected_binaries = (
        {"baseline_render", "candidate_render"}
        if render
        else {"baseline_ytt", "candidate_ytt", "sampler", "controller"}
    )
    binaries = manifest.get("binaries")
    binary_labels = set(binaries) if isinstance(binaries, dict) else set()
    if not isinstance(binaries, dict) or expected_binaries != binary_labels:
        missing = sorted(expected_binaries - binary_labels)
        extra = sorted(binary_labels - expected_binaries)
        raise ValueError(f"{path}: invalid binary identities; missing={missing}, extra={extra}")
    for label in sorted(expected_binaries):
        identity = binaries[label]
        if not isinstance(identity, dict):
            raise ValueError(f"{path}: binary identity {label!r} must be an object")
        binary = Path(str(identity.get("path", "")))
        if not binary.is_file():
            raise ValueError(f"{path}: manifested binary no longer exists: {binary}")
        require_artifact_value(path, f"{label} bytes", binary.stat().st_size, identity.get("bytes"))
        require_artifact_value(
            path, f"{label} SHA-256", sha256_file(binary), identity.get("sha256")
        )

    receipt_identity = manifest.get("build_receipt")
    if not isinstance(receipt_identity, dict):
        raise ValueError(f"{path}: build_receipt must be an object")
    receipt_path = (evidence_root / str(receipt_identity.get("path", ""))).resolve()
    try:
        receipt_path.relative_to(evidence_root)
    except ValueError as error:
        raise ValueError(f"{path}: build receipt escapes evidence root") from error
    current_receipt = identity_for_file(receipt_path)
    for field in ("bytes", "sha256"):
        require_artifact_value(path, f"build receipt {field}", receipt_identity.get(field), current_receipt[field])
    receipt = load_json_object(receipt_path)
    sources = receipt.get("sources")
    if not isinstance(sources, dict) or set(sources) != {"baseline", "candidate"}:
        raise ValueError(f"{receipt_path}: sources must contain exactly baseline and candidate")
    validate_build_receipt(
        receipt,
        Path(str(sources["baseline"].get("root", ""))),
        Path(str(sources["candidate"].get("root", ""))),
        render,
        refresh=False,
    )
    require_artifact_value(path, "receipt orchestrator", manifest.get("orchestrator"), receipt["orchestrator"])
    require_artifact_value(path, "receipt sources", manifest.get("sources"), sources)
    receipt_binaries = {
        label: {field: artifact[field] for field in ("path", "bytes", "sha256")}
        for label, artifact in receipt["artifacts"].items()
    }
    require_artifact_value(path, "receipt binaries", binaries, receipt_binaries)
    return manifest


def validate_render_document(
    document: dict[str, Any],
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
    run_contract: dict[str, Any] | None = None,
) -> None:
    require_artifact_value(path, "schema", document.get("schema"), "ytt.tui-perf.render.v1")
    require_artifact_value(path, "kind", document.get("kind"), "render_summary")
    require_artifact_value(path, "scenario SHA-256", document.get("scenario_sha256"), scenario_hash)
    require_artifact_value(path, "OS", normalized_os(document.get("os")), host_os)
    if run_contract is not None:
        require_artifact_value(path, "run ID", document.get("run_id"), run_contract["run_id"])
        started_unix_ns = non_negative_integer(
            document.get("started_unix_ns"), "render started_unix_ns", path
        )
        finished_unix_ns = non_negative_integer(
            document.get("finished_unix_ns"), "render finished_unix_ns", path
        )
        if not run_contract["started_unix_ns"] <= started_unix_ns < finished_unix_ns <= run_contract["finished_unix_ns"]:
            raise ValueError(f"{path}: render producer interval escapes its run contract")
    binary_label = f"{role}_render"
    expected_binary = manifest["binaries"][binary_label]["sha256"]
    require_artifact_value(
        path, "executed binary SHA-256", document.get("binary_sha256"), expected_binary
    )
    require_artifact_value(path, "batches_per_case", document.get("batches_per_case"), scenario["batches"])
    require_artifact_value(path, "draws_per_batch", document.get("draws_per_batch"), scenario["draws_per_batch"])
    cases = document.get("cases")
    if not isinstance(cases, list) or not cases:
        raise ValueError(f"{path}: render cases must be a non-empty array")
    names = [case.get("name") for case in cases if isinstance(case, dict)]
    if len(names) != len(cases) or len(names) != len(set(names)):
        raise ValueError(f"{path}: render case names must be unique objects")
    expected_measured = int(scenario["batches"]) * int(scenario["draws_per_batch"])
    for case in cases:
        require_artifact_value(path, f"{case['name']} warmup", case.get("warmup_draws"), scenario["warmup_draws"])
        require_artifact_value(path, f"{case['name']} measured draws", case.get("measured_draws"), expected_measured)
        batches = case.get("batches")
        if not isinstance(batches, list):
            raise ValueError(f"{path}: {case['name']} batches must be an array")
        require_artifact_value(path, f"{case['name']} batch count", len(batches), scenario["batches"])
        for batch in batches:
            if not isinstance(batch, dict):
                raise ValueError(f"{path}: {case['name']} contains a non-object batch")
            require_artifact_value(
                path,
                f"{case['name']} batch draws",
                batch.get("draws"),
                scenario["draws_per_batch"],
            )
    # Recompute every timing statistic from the exact raw histogram before accepting the
    # document; identity/count checks alone must not let a forged p95 reach comparison.
    render_metrics_from_document(document, path)


def process_run_directories(path: Path, scenario: dict[str, Any]) -> list[Path]:
    geometry = scenario["geometry"]
    if len(geometry) == 1:
        unexpected = sorted(item.name for item in path.glob("geometry-*") if item.is_dir())
        if unexpected:
            raise ValueError(f"{path}: single-geometry run has unexpected directories {unexpected}")
        return [path]
    expected_order = [f"geometry-{width}x{height}" for width, height in geometry]
    expected = set(expected_order)
    actual = {item.name for item in path.glob("geometry-*") if item.is_dir()}
    if actual != expected:
        raise ValueError(
            f"{path}: geometry directories are {sorted(actual)}, expected {sorted(expected)}"
        )
    return [path / name for name in expected_order]


def finite_non_negative_number(value: Any, label: str, path: Path) -> float:
    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(float(value))
        or float(value) < 0
    ):
        raise ValueError(f"{path}: {label} must be finite and non-negative")
    return float(value)


def non_negative_integer(value: Any, label: str, path: Path) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{path}: {label} must be a non-negative integer")
    return value


def validate_control_buffering(
    path: Path,
    records: list[dict[str, Any]],
    summary: dict[str, Any],
) -> None:
    require_artifact_value(path, "control summary schema", summary.get("schema"), "ytt.tui-perf.control.v1")
    summary_elapsed_ns = non_negative_integer(
        summary.get("elapsed_ns"), "summary elapsed_ns", path
    )
    buffering_cutoff_ns = non_negative_integer(
        summary.get("buffering_cutoff_ns"), "summary buffering_cutoff_ns", path
    )
    if buffering_cutoff_ns > summary_elapsed_ns:
        raise ValueError(f"{path}: buffering cutoff occurs after the summary boundary")
    summary_events = non_negative_integer(
        summary.get("buffering_events"), "summary buffering_events", path
    )
    summary_ms = non_negative_integer(
        summary.get("buffering_ms"), "summary buffering_ms", path
    )

    buffering_started_ns: int | None = None
    buffering_events = 0
    buffering_total_ns = 0
    previous_elapsed_ns = -1
    for index, record in enumerate(records):
        if record.get("kind") != "mpv_event":
            continue
        require_artifact_value(
            path,
            f"mpv event {index} schema",
            record.get("schema"),
            "ytt.tui-perf.control.v1",
        )
        elapsed_ns = non_negative_integer(
            record.get("elapsed_ns"), f"mpv event {index} elapsed_ns", path
        )
        if elapsed_ns < previous_elapsed_ns:
            raise ValueError(f"{path}: mpv event elapsed_ns values must be monotonic")
        if elapsed_ns > summary_elapsed_ns:
            raise ValueError(f"{path}: mpv event occurs after the summary boundary")
        previous_elapsed_ns = elapsed_ns
        event = record.get("event")
        if not isinstance(event, dict):
            raise ValueError(f"{path}: mpv event {index} payload must be an object")
        if (
            event.get("event") != "property-change"
            or event.get("name") != "paused-for-cache"
            or not isinstance(event.get("data"), bool)
        ):
            continue
        if elapsed_ns >= buffering_cutoff_ns:
            if buffering_started_ns is not None:
                buffering_total_ns += buffering_cutoff_ns - buffering_started_ns
                buffering_started_ns = None
            continue
        if event["data"] and buffering_started_ns is None:
            buffering_started_ns = elapsed_ns
            buffering_events += 1
        elif not event["data"] and buffering_started_ns is not None:
            buffering_total_ns += elapsed_ns - buffering_started_ns
            buffering_started_ns = None
    if buffering_started_ns is not None:
        buffering_total_ns += buffering_cutoff_ns - buffering_started_ns

    require_artifact_value(
        path, "recomputed buffering_events", summary_events, buffering_events
    )
    require_artifact_value(
        path,
        "recomputed buffering_ms",
        summary_ms,
        buffering_total_ns // 1_000_000,
    )


def validate_control_time_pos_summary(
    path: Path,
    records: list[dict[str, Any]],
    summary: dict[str, Any],
) -> tuple[int, float, int, float] | None:
    cutoff_ns = non_negative_integer(
        summary.get("buffering_cutoff_ns"), "summary buffering_cutoff_ns", path
    )
    first: tuple[int, float] | None = None
    last: tuple[int, float] | None = None
    for index, record in enumerate(records):
        if record.get("kind") != "mpv_event":
            continue
        elapsed_ns = non_negative_integer(
            record.get("elapsed_ns"), f"mpv event {index} elapsed_ns", path
        )
        if elapsed_ns > cutoff_ns:
            continue
        event = record.get("event")
        if not isinstance(event, dict) or (
            event.get("event") != "property-change"
            or event.get("name") != "time-pos"
        ):
            continue
        value = event.get("data")
        if value is None:
            continue
        position = finite_non_negative_number(
            value, f"mpv event {index} time-pos", path
        )
        if first is None:
            first = (elapsed_ns, position)
        last = (elapsed_ns, position)

    expected = {
        "cutoff_first_time_pos_ns": first[0] if first else None,
        "cutoff_first_time_pos_s": first[1] if first else None,
        "cutoff_last_time_pos_ns": last[0] if last else None,
        "cutoff_last_time_pos_s": last[1] if last else None,
    }
    for field, value in expected.items():
        require_artifact_value(path, f"recomputed {field}", summary.get(field), value)
    if first is None or last is None:
        return None
    return first[0], first[1], last[0], last[1]


def validate_steady_playback_progress(
    path: Path,
    records: list[dict[str, Any]],
    summary: dict[str, Any],
    scenario: dict[str, Any],
) -> None:
    fraction = scenario.get("minimum_playback_progress_fraction")
    if fraction is None:
        return
    progress = validate_control_time_pos_summary(path, records, summary)
    if progress is None:
        raise ValueError(f"{path}: steady playback has no time-pos evidence")
    first_ns, first_s, last_ns, last_s = progress
    cutoff_ns = non_negative_integer(
        summary.get("buffering_cutoff_ns"), "summary buffering_cutoff_ns", path
    )
    tail_tolerance_s = finite_non_negative_number(
        scenario.get("time_pos_tail_tolerance_s"),
        "time_pos_tail_tolerance_s",
        path,
    )
    tail_tolerance_ns = int(tail_tolerance_s * 1_000_000_000)
    if last_ns + tail_tolerance_ns < cutoff_ns:
        raise ValueError(
            f"{path}: final time-pos evidence is not within {tail_tolerance_s:g}s "
            "of the fixed observation cutoff"
        )
    minimum_progress_s = cutoff_ns / 1_000_000_000 * float(fraction)
    actual_progress_s = last_s - first_s
    if actual_progress_s < minimum_progress_s:
        raise ValueError(
            f"{path}: steady playback advanced {actual_progress_s:g}s from "
            f"{first_ns}ns to {last_ns}ns; requires at least {minimum_progress_s:g}s"
        )


def normalized_seek_target_ms(value: Any, label: str, path: Path) -> int:
    seconds = finite_non_negative_number(value, label, path)
    scaled = seconds * 1_000.0
    if not math.isfinite(scaled) or scaled >= 1 << 64:
        raise ValueError(f"{path}: {label} exceeds the controller transport range")
    milliseconds = math.floor(scaled + 0.5)
    if milliseconds >= 1 << 64:
        raise ValueError(f"{path}: {label} exceeds the controller transport range")
    return milliseconds


def controller_scheduled_offset_ns(
    observation_ns: int, ordinal: int, total_actions: int
) -> int:
    if observation_ns == 0 or ordinal == 0 or total_actions == 0:
        return 0
    if not 0 < ordinal <= total_actions:
        raise ValueError("controller schedule ordinal is outside the action count")
    return observation_ns * ordinal // (total_actions + 1)


def validate_control_operations(
    path: Path,
    records: list[dict[str, Any]],
    summary_elapsed_ns: int,
    scenario: dict[str, Any],
) -> None:
    operation_entries = [
        (stream_index, record)
        for stream_index, record in enumerate(records)
        if record.get("kind") == "operation"
    ]
    operation_windows: list[tuple[int, int, str, dict[str, Any]]] = []
    actual_seek_targets_ms: list[int] = []
    previous_completed_ns: int | None = None
    for index, (stream_index, record) in enumerate(operation_entries):
        require_artifact_value(
            path,
            f"operation {index} schema",
            record.get("schema"),
            "ytt.tui-perf.control.v1",
        )
        started_ns = non_negative_integer(
            record.get("operation_started_ns"), f"operation {index} started", path
        )
        completed_ns = non_negative_integer(
            record.get("operation_completed_ns"), f"operation {index} completed", path
        )
        if completed_ns < started_ns or completed_ns > summary_elapsed_ns:
            raise ValueError(f"{path}: operation {index} timestamps are outside observation")
        if previous_completed_ns is not None and started_ns < previous_completed_ns:
            raise ValueError(f"{path}: operation {index} overlaps its predecessor")
        previous_completed_ns = completed_ns
        actual_latency = finite_non_negative_number(
            record.get("latency_ms"), f"operation {index} latency_ms", path
        )
        expected_latency = (completed_ns - started_ns) / 1_000_000.0
        if not math.isclose(
            actual_latency, expected_latency, rel_tol=1e-12, abs_tol=1e-12
        ):
            raise ValueError(
                f"{path}: operation {index} latency does not match raw timestamps"
            )
        completion_source = record.get("completion_source")
        completion_event_ns = record.get("completion_event_elapsed_ns")
        completion_event_payload: dict[str, Any] | None = None
        if completion_source == "mpv_event":
            require_artifact_value(
                path,
                f"operation {index} completion timestamp",
                completion_event_ns,
                completed_ns,
            )
            matches = [
                (event_index, event)
                for event_index, event in enumerate(records)
                if event.get("kind") == "mpv_event"
                and event.get("elapsed_ns") == completed_ns
            ]
            if len(matches) != 1:
                raise ValueError(
                    f"{path}: operation {index} completion event is not unique"
                )
            event_index, event_record = matches[0]
            if event_index >= stream_index:
                raise ValueError(
                    f"{path}: operation {index} completion event is out of stream order"
                )
            event = event_record.get("event")
            if not isinstance(event, dict):
                raise ValueError(f"{path}: operation {index} completion event is malformed")
            completion_event_payload = event
            require_artifact_value(
                path,
                f"operation {index} event type",
                record.get("completion_event_type"),
                event.get("event"),
            )
            require_artifact_value(
                path,
                f"operation {index} property",
                record.get("completion_property"),
                event.get("name"),
            )
        elif completion_source == "status":
            require_artifact_value(
                path,
                f"operation {index} event timestamp",
                completion_event_ns,
                None,
            )
            require_artifact_value(
                path,
                f"operation {index} event type",
                record.get("completion_event_type"),
                None,
            )
            require_artifact_value(
                path,
                f"operation {index} property",
                record.get("completion_property"),
                None,
            )
        else:
            raise ValueError(f"{path}: operation {index} has invalid completion source")

        operation_name = record.get("operation")
        if not isinstance(operation_name, str):
            raise ValueError(f"{path}: operation {index} name must be a string")
        detail = record.get("detail")
        if not isinstance(detail, dict):
            raise ValueError(f"{path}: operation {index} detail must be an object")
        if operation_name in {"resume_session", "play_query"}:
            require_artifact_value(
                path,
                f"operation {index} load completion",
                record.get("completion_event_type"),
                "playback-restart",
            )
        elif operation_name == "seek":
            require_artifact_value(
                path,
                f"operation {index} seek property",
                record.get("completion_property"),
                "time-pos",
            )
            target = finite_non_negative_number(
                detail.get("target_s"), "seek target", path
            )
            actual_seek_targets_ms.append(
                normalized_seek_target_ms(target, "seek target", path)
            )
            observed = finite_non_negative_number(
                detail.get("observed_target_s"), "seek observed target", path
            )
            if completion_event_payload is None:
                raise ValueError(f"{path}: seek completion has no raw mpv event payload")
            raw_observed = finite_non_negative_number(
                completion_event_payload.get("data"),
                "seek raw time-pos",
                path,
            )
            require_artifact_value(
                path,
                f"operation {index} raw seek observation",
                raw_observed,
                observed,
            )
            tolerance = finite_non_negative_number(
                detail.get("target_tolerance_s"), "seek tolerance", path
            )
            normalized_target_s = (
                normalized_seek_target_ms(target, "seek target", path) / 1_000.0
            )
            if tolerance != 2.0 or abs(raw_observed - normalized_target_s) > tolerance:
                raise ValueError(
                    f"{path}: raw seek completion is not bound to its normalized target"
                )
            restart_ns = non_negative_integer(
                detail.get("playback_restart_elapsed_ns"),
                "seek restart timestamp",
                path,
            )
            if not started_ns <= restart_ns <= completed_ns:
                raise ValueError(f"{path}: seek restart is outside the operation")
            restart_matches = [
                event_index
                for event_index, event in enumerate(records)
                if event.get("kind") == "mpv_event"
                and event.get("elapsed_ns") == restart_ns
                and isinstance(event.get("event"), dict)
                and event["event"].get("event") == "playback-restart"
            ]
            if len(restart_matches) != 1 or restart_matches[0] >= stream_index:
                raise ValueError(f"{path}: seek restart proof is missing or out of order")
        elif operation_name == "pause":
            require_artifact_value(
                path, f"operation {index} pause source", completion_source, "status"
            )
            require_artifact_value(
                path,
                f"operation {index} pause status field",
                detail.get("status_field"),
                "paused",
            )
            require_artifact_value(
                path,
                f"operation {index} pause status value",
                detail.get("status_value"),
                True,
            )
        elif operation_name == "resume":
            require_artifact_value(
                path,
                f"operation {index} resume property",
                record.get("completion_property"),
                "time-pos",
            )
            if completion_event_payload is None:
                raise ValueError(f"{path}: resume completion has no raw mpv event payload")
            completion_position = finite_non_negative_number(
                completion_event_payload.get("data"),
                "resume raw completion time-pos",
                path,
            )
            pre_resume_positions: list[tuple[int, float]] = []
            for event_index, event_record in enumerate(records[:stream_index]):
                payload = event_record.get("event")
                if (
                    event_record.get("kind") != "mpv_event"
                    or not isinstance(payload, dict)
                    or payload.get("event") != "property-change"
                    or payload.get("name") != "time-pos"
                ):
                    continue
                elapsed_ns = non_negative_integer(
                    event_record.get("elapsed_ns"),
                    "pre-resume time-pos elapsed_ns",
                    path,
                )
                if elapsed_ns <= started_ns:
                    pre_resume_positions.append(
                        (
                            event_index,
                            finite_non_negative_number(
                                payload.get("data"),
                                "pre-resume raw time-pos",
                                path,
                            ),
                        )
                    )
            if not pre_resume_positions:
                raise ValueError(f"{path}: resume has no raw pre-resume time-pos boundary")
            _boundary_event_index, paused_boundary = pre_resume_positions[-1]
            paused_detail = finite_non_negative_number(
                detail.get("paused_time_pos_s"), "resume paused time-pos detail", path
            )
            require_artifact_value(
                path,
                f"operation {index} raw paused boundary",
                paused_boundary,
                paused_detail,
            )
            if completion_position <= paused_boundary + CONTROL_MIN_RESUME_PROGRESS_S:
                raise ValueError(
                    f"{path}: raw resume completion does not advance beyond the paused boundary"
                )
        operation_windows.append((started_ns, completed_ns, operation_name, detail))

    expected_load = scenario.get("controller_load")
    expected_load_operation = {
        "resume-session": "resume_session",
        "none": "ready",
    }.get(expected_load)
    if expected_load_operation is None:
        raise ValueError(f"{path}: controller load policy is invalid")
    expected_operations = [expected_load_operation]
    expected_operations.extend("seek" for _ in scenario.get("seeks_s", []))
    if scenario["pause_policy"] == "pause-resume":
        expected_operations.extend(("pause", "resume"))
    operations = [name for _started, _completed, name, _detail in operation_windows]
    require_artifact_value(
        path, "exact controller operation order", operations, expected_operations
    )

    expected_seek_targets_ms = [
        normalized_seek_target_ms(value, "scenario seek target", path)
        for value in scenario.get("seeks_s", [])
    ]
    require_artifact_value(
        path,
        "normalized controller seek targets",
        actual_seek_targets_ms,
        expected_seek_targets_ms,
    )

    observation_s = finite_non_negative_number(
        scenario.get("warmup_s"), "scenario warmup_s", path
    ) + finite_non_negative_number(
        scenario.get("sample_s"), "scenario sample_s", path
    )
    observation_ns = int(observation_s * 1_000_000_000)
    scheduled_action_count = len(expected_seek_targets_ms) + int(
        scenario["pause_policy"] == "pause-resume"
    )
    scheduled_starts: list[tuple[int, int, str]] = [
        (0, 0, expected_load_operation)
    ]
    scheduled_starts.extend(
        (
            seek_index + 1,
            controller_scheduled_offset_ns(
                observation_ns, seek_index + 1, scheduled_action_count
            ),
            "seek",
        )
        for seek_index in range(len(expected_seek_targets_ms))
    )
    if scenario["pause_policy"] == "pause-resume":
        scheduled_starts.append(
            (
                len(expected_seek_targets_ms) + 1,
                controller_scheduled_offset_ns(
                    observation_ns, scheduled_action_count, scheduled_action_count
                ),
                "pause",
            )
        )
    for operation_index, expected_start_ns, operation_name in scheduled_starts:
        actual_start_ns = operation_windows[operation_index][0]
        latest_start_ns = (
            expected_start_ns + CONTROL_ACTION_SCHEDULE_LATE_TOLERANCE_NS
        )
        if not expected_start_ns <= actual_start_ns <= latest_start_ns:
            raise ValueError(
                f"{path}: {operation_name} operation {operation_index} started at "
                f"{actual_start_ns}ns, outside its scheduled "
                f"{expected_start_ns}..{latest_start_ns}ns window"
            )

    if scenario["pause_policy"] == "pause-resume":
        pause_index = len(expected_operations) - 2
        resume_index = len(expected_operations) - 1
        _pause_started, pause_completed, _pause_name, _pause_detail = operation_windows[
            pause_index
        ]
        resume_started, _resume_completed, _resume_name, resume_detail = operation_windows[
            resume_index
        ]
        hold_ms = non_negative_integer(
            scenario.get("pause_hold_ms"), "scenario pause_hold_ms", path
        )
        require_artifact_value(
            path, "resume pause hold detail", resume_detail.get("pause_hold_ms"), hold_ms
        )
        expected_gap_ns = hold_ms * 1_000_000
        actual_gap_ns = resume_started - pause_completed
        if not (
            expected_gap_ns
            <= actual_gap_ns
            <= expected_gap_ns + CONTROL_PAUSE_HOLD_LATE_TOLERANCE_NS
        ):
            raise ValueError(
                f"{path}: confirmed pause interval {actual_gap_ns}ns is outside "
                f"{expected_gap_ns}.."
                f"{expected_gap_ns + CONTROL_PAUSE_HOLD_LATE_TOLERANCE_NS}ns"
            )


def control_operations_self_test() -> None:
    schema = "ytt.tui-perf.control.v1"

    def event(
        elapsed_ns: int,
        event_type: str,
        name: str | None = None,
        data: Any = None,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {"event": event_type}
        if name is not None:
            payload["name"] = name
            payload["data"] = data
        return {
            "schema": schema,
            "kind": "mpv_event",
            "elapsed_ns": elapsed_ns,
            "event": payload,
        }

    def operation(
        name: str,
        started_ns: int,
        completed_ns: int,
        source: str,
        event_type: str | None,
        property_name: str | None,
        detail: dict[str, Any],
    ) -> dict[str, Any]:
        return {
            "schema": schema,
            "kind": "operation",
            "operation": name,
            "operation_started_ns": started_ns,
            "operation_completed_ns": completed_ns,
            "latency_ms": (completed_ns - started_ns) / 1_000_000.0,
            "completion_source": source,
            "completion_event_elapsed_ns": completed_ns if source == "mpv_event" else None,
            "completion_event_type": event_type,
            "completion_property": property_name,
            "detail": detail,
        }

    records = [
        event(20_000_000, "playback-restart"),
        operation(
            "resume_session",
            10_000_000,
            20_000_000,
            "mpv_event",
            "playback-restart",
            None,
            {},
        ),
        event(35_000_000, "playback-restart"),
        event(40_000_000, "property-change", "time-pos", 15.0),
        operation(
            "seek",
            30_000_000,
            40_000_000,
            "mpv_event",
            "property-change",
            "time-pos",
            {
                "target_s": 15.0,
                "observed_target_s": 15.0,
                "target_tolerance_s": 2.0,
                "playback_restart_elapsed_ns": 35_000_000,
            },
        ),
        event(55_000_000, "playback-restart"),
        event(60_000_000, "property-change", "time-pos", 90.0),
        operation(
            "seek",
            50_000_000,
            60_000_000,
            "mpv_event",
            "property-change",
            "time-pos",
            {
                "target_s": 90.0,
                "observed_target_s": 90.0,
                "target_tolerance_s": 2.0,
                "playback_restart_elapsed_ns": 55_000_000,
            },
        ),
        operation(
            "pause",
            75_000_000,
            80_000_000,
            "status",
            None,
            None,
            {"status_field": "paused", "status_value": True},
        ),
        event(100_000_000, "property-change", "time-pos", 90.0),
        event(600_000_000, "property-change", "time-pos", 90.02),
        operation(
            "resume",
            580_000_000,
            600_000_000,
            "mpv_event",
            "property-change",
            "time-pos",
            {"pause_hold_ms": 500, "paused_time_pos_s": 90.0},
        ),
    ]
    scenario = {
        "controller_load": "resume-session",
        "seeks_s": [15, 90],
        "pause_policy": "pause-resume",
        "pause_hold_ms": 500,
        "warmup_s": 0,
        "sample_s": 0.1,
    }
    path = Path("<control-operation-self-test>")
    validate_control_operations(path, records, 700_000_000, scenario)

    def expect_rejected(
        label: str,
        tampered: list[dict[str, Any]],
        summary_elapsed_ns: int = 700_000_000,
        expected_error: str | None = None,
    ) -> None:
        try:
            validate_control_operations(path, tampered, summary_elapsed_ns, scenario)
        except ValueError as error:
            if expected_error is not None and expected_error not in str(error):
                raise AssertionError(
                    f"controller operation tampering {label!r} failed for the wrong "
                    f"reason: {error}"
                ) from error
        else:
            raise AssertionError(f"controller operation tampering was accepted: {label}")

    def shift_suffix(start_ns: int) -> list[dict[str, Any]]:
        shifted = json.loads(json.dumps(records))
        delta_ns = CONTROL_ACTION_SCHEDULE_LATE_TOLERANCE_NS + 1
        for record in shifted:
            if record.get("kind") == "mpv_event":
                if record["elapsed_ns"] >= start_ns:
                    record["elapsed_ns"] += delta_ns
                continue
            if record.get("kind") != "operation":
                continue
            if record["operation_started_ns"] < start_ns:
                continue
            record["operation_started_ns"] += delta_ns
            record["operation_completed_ns"] += delta_ns
            completion_ns = record.get("completion_event_elapsed_ns")
            if completion_ns is not None:
                record["completion_event_elapsed_ns"] = completion_ns + delta_ns
            restart_ns = record.get("detail", {}).get("playback_restart_elapsed_ns")
            if restart_ns is not None:
                record["detail"]["playback_restart_elapsed_ns"] = restart_ns + delta_ns
        return shifted

    late_load = shift_suffix(0)
    expect_rejected(
        "late immediate load",
        late_load,
        900_000_000,
        "resume_session operation 0 started",
    )

    early_seek = json.loads(json.dumps(records))
    first_seek = next(
        record
        for record in early_seek
        if record.get("kind") == "operation" and record.get("operation") == "seek"
    )
    first_seek["operation_started_ns"] = 24_999_999
    first_seek["latency_ms"] = (
        first_seek["operation_completed_ns"] - first_seek["operation_started_ns"]
    ) / 1_000_000.0
    expect_rejected(
        "early scheduled seek",
        early_seek,
        expected_error="seek operation 1 started",
    )

    late_seek = shift_suffix(30_000_000)
    expect_rejected(
        "late scheduled seek",
        late_seek,
        900_000_000,
        "seek operation 1 started",
    )

    late_pause = shift_suffix(75_000_000)
    expect_rejected(
        "late scheduled pause",
        late_pause,
        900_000_000,
        "pause operation 3 started",
    )

    target_swap = json.loads(json.dumps(records))
    target_operations = [
        record
        for record in target_swap
        if record.get("kind") == "operation" and record.get("operation") == "seek"
    ]
    for record, target in zip(target_operations, (90.0, 15.0)):
        record["detail"]["target_s"] = target
        record["detail"]["observed_target_s"] = target
        completion_event = next(
            event_record
            for event_record in target_swap
            if event_record.get("kind") == "mpv_event"
            and event_record.get("elapsed_ns") == record["operation_completed_ns"]
        )
        completion_event["event"]["data"] = target
    expect_rejected("seek target swap", target_swap)

    wrong_order = json.loads(json.dumps(records))
    pause_stream_index = next(
        index
        for index, record in enumerate(wrong_order)
        if record.get("operation") == "pause"
    )
    final_seek_stream_index = [
        index
        for index, record in enumerate(wrong_order)
        if record.get("operation") == "seek"
    ][-1]
    pause_record = wrong_order.pop(pause_stream_index)
    pause_record["operation_started_ns"] = 45_000_000
    pause_record["operation_completed_ns"] = 46_000_000
    pause_record["latency_ms"] = 1.0
    wrong_order.insert(final_seek_stream_index, pause_record)
    expect_rejected("operation order", wrong_order)

    overlap = json.loads(json.dumps(records))
    seek_operations = [
        record
        for record in overlap
        if record.get("kind") == "operation" and record.get("operation") == "seek"
    ]
    seek_operations[1]["operation_started_ns"] = 39_999_999
    seek_operations[1]["latency_ms"] = (
        seek_operations[1]["operation_completed_ns"]
        - seek_operations[1]["operation_started_ns"]
    ) / 1_000_000.0
    expect_rejected("overlapping operations", overlap)

    short_pause = json.loads(json.dumps(records))
    resume = next(record for record in short_pause if record.get("operation") == "resume")
    resume["operation_started_ns"] = 579_999_999
    resume["latency_ms"] = (
        resume["operation_completed_ns"] - resume["operation_started_ns"]
    ) / 1_000_000.0
    expect_rejected("short pause", short_pause)

    long_pause = json.loads(json.dumps(records))
    resume = next(record for record in long_pause if record.get("operation") == "resume")
    old_completion_ns = resume["operation_completed_ns"]
    resume["operation_started_ns"] = (
        80_000_000 + 500_000_000 + CONTROL_PAUSE_HOLD_LATE_TOLERANCE_NS + 1
    )
    resume["operation_completed_ns"] = resume["operation_started_ns"] + 20_000_000
    resume["completion_event_elapsed_ns"] = resume["operation_completed_ns"]
    resume["latency_ms"] = 20.0
    completion_event = next(
        record
        for record in long_pause
        if record.get("kind") == "mpv_event"
        and record.get("elapsed_ns") == old_completion_ns
    )
    completion_event["elapsed_ns"] = resume["operation_completed_ns"]
    expect_rejected("long pause", long_pause, 800_000_000)

    detail_tamper = json.loads(json.dumps(records))
    resume = next(record for record in detail_tamper if record.get("operation") == "resume")
    resume["detail"]["pause_hold_ms"] = 499
    expect_rejected("pause detail", detail_tamper)

    raw_seek_tamper = json.loads(json.dumps(records))
    seek_event = next(
        record
        for record in raw_seek_tamper
        if record.get("kind") == "mpv_event" and record.get("elapsed_ns") == 40_000_000
    )
    seek_event["event"]["data"] = 14.5
    expect_rejected("raw seek payload", raw_seek_tamper)

    raw_resume_tamper = json.loads(json.dumps(records))
    resume_event = next(
        record
        for record in raw_resume_tamper
        if record.get("kind") == "mpv_event" and record.get("elapsed_ns") == 600_000_000
    )
    resume_event["event"]["data"] = 90.005
    expect_rejected("raw resume payload", raw_resume_tamper)

    raw_boundary_tamper = json.loads(json.dumps(records))
    boundary_event = next(
        record
        for record in raw_boundary_tamper
        if record.get("kind") == "mpv_event" and record.get("elapsed_ns") == 100_000_000
    )
    boundary_event["event"]["data"] = 89.0
    expect_rejected("raw paused boundary", raw_boundary_tamper)


def portable_executable_stem(value: str) -> str:
    basename = value.replace("\\", "/").rsplit("/", 1)[-1].lower()
    return basename[:-4] if basename.endswith(".exe") else basename.rsplit(".", 1)[0]


def raw_process_role(root_pid: int, process: dict[str, Any]) -> str:
    if process["pid"] == root_pid:
        return "ytt"
    name = process["name"].lower()
    command = process["command"]
    argv0 = portable_executable_stem(command[0]) if command else ""
    return "mpv" if name in {"mpv", "mpv.exe"} or argv0 == "mpv" else "other"


def mpv_last_option(command: list[str], option: str) -> str | None:
    value: str | None = None
    index = 0
    prefix = f"--{option}="
    split = f"--{option}"
    while index < len(command):
        argument = command[index]
        if argument.startswith(prefix):
            value = argument[len(prefix) :]
        elif argument == split:
            if index + 1 < len(command):
                index += 1
                value = command[index]
            else:
                value = None
        index += 1
    return value


def require_effective_mpv_cache_args(
    path: Path,
    command: list[str],
    expected: dict[str, str],
    context: str,
) -> None:
    if expected != REQUIRED_PLAYBACK_MPV_CACHE_ARGS:
        raise ValueError(f"{path}: {context} has no valid expected mpv cache argv contract")
    for argument, expected_value in expected.items():
        actual = mpv_last_option(command, argument.removeprefix("--"))
        if actual != expected_value:
            raise ValueError(
                f"{path}: {context} effective {argument} is {actual!r}, "
                f"expected {expected_value!r} by last-option-wins"
            )


def mpv_cache_argv_contract_self_test() -> None:
    path = Path("<mpv-cache-argv-self-test>")
    valid = [
        "mpv",
        "--demuxer-max-bytes=1MiB",
        "--demuxer-max-back-bytes",
        "1MiB",
        "--demuxer-max-bytes",
        "32MiB",
        "--demuxer-max-back-bytes=8MiB",
    ]
    require_effective_mpv_cache_args(
        path, valid, REQUIRED_PLAYBACK_MPV_CACHE_ARGS, "valid command"
    )

    def expect_rejected(label: str, command: list[str]) -> None:
        try:
            require_effective_mpv_cache_args(
                path, command, REQUIRED_PLAYBACK_MPV_CACHE_ARGS, label
            )
        except ValueError:
            pass
        else:
            raise AssertionError(f"mpv cache argv contract accepted {label}")

    expect_rejected(
        "missing backward cache option",
        ["mpv", "--demuxer-max-bytes=32MiB"],
    )
    expect_rejected(
        "later forward cache override",
        [*valid, "--demuxer-max-bytes=16MiB"],
    )


def silent_mpv_command(command: list[str]) -> bool:
    ao = mpv_last_option(command, "ao")
    volume = mpv_last_option(command, "volume")
    try:
        numeric_volume = float(volume) if volume is not None else None
    except ValueError:
        numeric_volume = None
    return ao is not None and ao.lower() == "null" and numeric_volume == 0.0


def mpv_ipc_identity(command: list[str]) -> tuple[list[str] | None, str | None]:
    identity: list[str] = []
    endpoint: str | None = None
    index = 0
    while index < len(command):
        argument = command[index]
        if argument.startswith("--input-ipc-server="):
            value = argument.split("=", 1)[1]
            identity.append(argument)
            if value:
                endpoint = value
        elif argument == "--input-ipc-server":
            if index + 1 >= len(command):
                return None, None
            value = command[index + 1]
            identity.extend((argument, value))
            if value:
                endpoint = value
            index += 1
        index += 1
    return (identity, endpoint) if identity and endpoint else (None, None)


def validate_measured_samples(
    path: Path,
    header: dict[str, Any],
    summary: dict[str, Any],
    all_samples: list[dict[str, Any]],
    require_mpv: bool,
    expected_mpv_cache_args: dict[str, str] | None = None,
) -> dict[str, Any]:
    if require_mpv and expected_mpv_cache_args != REQUIRED_PLAYBACK_MPV_CACHE_ARGS:
        raise ValueError(f"{path}: playback samples have no valid expected mpv cache argv contract")
    if not require_mpv and expected_mpv_cache_args is not None:
        raise ValueError(f"{path}: non-playback samples cannot declare expected mpv cache argv")
    warmup_ms = non_negative_integer(header.get("warmup_ms"), "warmup_ms", path)
    duration_ms = non_negative_integer(header.get("duration_ms"), "duration_ms", path)
    interval_ms = non_negative_integer(header.get("interval_ms"), "interval_ms", path)
    if interval_ms == 0:
        raise ValueError(f"{path}: interval_ms must be positive")
    require_artifact_value(
        path, "CPU accounting method", header.get("cpu_accounting"), CPU_ACCOUNTING_METHOD
    )
    cpu_window_start_ns = non_negative_integer(
        header.get("cpu_window_start_ns"), "cpu_window_start_ns", path
    )
    cpu_window_end_ns = non_negative_integer(
        header.get("cpu_window_end_ns"), "cpu_window_end_ns", path
    )
    if cpu_window_end_ns <= cpu_window_start_ns:
        raise ValueError(f"{path}: CPU measurement window must be positive")
    cpu_window_duration_ns = cpu_window_end_ns - cpu_window_start_ns
    require_artifact_value(
        path, "CPU window warmup milliseconds", cpu_window_start_ns // 1_000_000, warmup_ms
    )
    require_artifact_value(
        path,
        "CPU window duration milliseconds",
        cpu_window_duration_ns // 1_000_000,
        duration_ms,
    )
    require_artifact_value(
        path, "summary CPU accounting method", summary.get("cpu_accounting"), CPU_ACCOUNTING_METHOD
    )
    require_artifact_value(
        path,
        "summary CPU window start",
        summary.get("cpu_window_start_ns"),
        cpu_window_start_ns,
    )
    require_artifact_value(
        path,
        "summary CPU window end",
        summary.get("cpu_window_end_ns"),
        cpu_window_end_ns,
    )
    measured = [record for record in all_samples if record.get("phase") == "measure"]
    if not measured:
        raise ValueError(f"{path}: no measured samples")
    expected_samples = max(
        2, math.ceil(cpu_window_duration_ns / (interval_ms * 1_000_000)) + 1
    )
    if not expected_samples - 1 <= len(measured) <= expected_samples + 1:
        raise ValueError(
            f"{path}: measured sample count {len(measured)} is outside "
            f"{expected_samples}±1"
        )
    root_pid = non_negative_integer(header.get("root_pid"), "root_pid", path)
    if root_pid == 0:
        raise ValueError(f"{path}: root_pid must be positive")
    elapsed = []
    role_points: dict[str, list[tuple[float, int, int]]] = {}
    previous_by_identity: dict[tuple[int, int], tuple[int, int]] = {}
    start_by_pid: dict[int, int] = {}
    recomputed_samples: list[dict[str, Any]] = []
    last_mpv_identities: list[dict[str, Any]] = []
    proof = {
        "samples": 0,
        "samples_with_mpv": 0,
        "samples_all_silent": 0,
        "samples_all_cleanup_identified": 0,
    }
    previous_observed_ns = -1
    total_cpu_overlap_ns = 0
    for index, record in enumerate(all_samples):
        require_artifact_value(path, f"sample {index} schema", record.get("schema"), "ytt.tui-perf.samples.v1")
        observed_ns = non_negative_integer(
            record.get("observed_elapsed_ns"), f"sample {index} observed_elapsed_ns", path
        )
        if observed_ns <= previous_observed_ns:
            raise ValueError(f"{path}: sample observed_elapsed_ns must be strictly increasing")
        expected_cpu_overlap_ns = 0
        if previous_observed_ns >= 0:
            expected_cpu_overlap_ns = max(
                0,
                min(observed_ns, cpu_window_end_ns)
                - max(previous_observed_ns, cpu_window_start_ns),
            )
        recorded_cpu_overlap_ns = non_negative_integer(
            record.get("cpu_interval_overlap_ns"),
            f"sample {index} cpu_interval_overlap_ns",
            path,
        )
        require_artifact_value(
            path,
            f"sample {index} CPU interval overlap",
            recorded_cpu_overlap_ns,
            expected_cpu_overlap_ns,
        )
        total_cpu_overlap_ns += expected_cpu_overlap_ns
        previous_observed_ns = observed_ns
        raw_elapsed = non_negative_integer(record.get("elapsed_ms"), f"sample {index} elapsed_ms", path)
        require_artifact_value(path, f"sample {index} elapsed_ms", raw_elapsed, observed_ns // 1_000_000)
        expected_phase = "warmup" if observed_ns < cpu_window_start_ns else "measure"
        require_artifact_value(path, f"sample {index} phase", record.get("phase"), expected_phase)
        processes = record.get("processes")
        if not isinstance(processes, list) or not processes:
            raise ValueError(f"{path}: sample {index} processes must be a non-empty array")
        seen_pids: set[int] = set()
        parent_by_pid: dict[int, int | None] = {}
        computed_roles: dict[str, dict[str, int | float]] = {}
        sample_mpv: list[dict[str, Any]] = []
        for process_index, process in enumerate(processes):
            if not isinstance(process, dict):
                raise ValueError(f"{path}: sample {index} process {process_index} is not an object")
            expected_fields = {
                "pid", "parent_pid", "role", "name", "start_time_unix_s",
                "accumulated_cpu_ms", "cpu_percent", "rss_bytes", "command",
                "executable", "executable_bytes", "executable_sha256",
            }
            if set(process) != expected_fields:
                raise ValueError(
                    f"{path}: sample {index} process {process_index} fields are "
                    f"{sorted(process)}, expected {sorted(expected_fields)}"
                )
            pid = non_negative_integer(process.get("pid"), "process pid", path)
            if pid == 0 or pid in seen_pids:
                raise ValueError(f"{path}: sample {index} has duplicate/zero process PID {pid}")
            seen_pids.add(pid)
            parent_pid = process.get("parent_pid")
            if parent_pid is not None:
                non_negative_integer(parent_pid, "process parent_pid", path)
            parent_by_pid[pid] = parent_pid
            name = process.get("name")
            command = process.get("command")
            if not isinstance(name, str) or not name:
                raise ValueError(f"{path}: sample {index} process {pid} has invalid name")
            if not isinstance(command, list) or not all(isinstance(item, str) for item in command):
                raise ValueError(f"{path}: sample {index} process {pid} has invalid command")
            start_time = non_negative_integer(process.get("start_time_unix_s"), "process start time", path)
            if pid in start_by_pid and start_by_pid[pid] != start_time:
                raise ValueError(f"{path}: PID {pid} was reused across raw samples")
            start_by_pid[pid] = start_time
            accumulated = non_negative_integer(process.get("accumulated_cpu_ms"), "accumulated CPU", path)
            rss = non_negative_integer(process.get("rss_bytes"), "process RSS", path)
            identity = (pid, start_time)
            previous = previous_by_identity.get(identity)
            if previous is None:
                expected_cpu = 0.0
            else:
                previous_accumulated, previous_ns = previous
                if accumulated < previous_accumulated:
                    raise ValueError(f"{path}: accumulated CPU decreased for PID {pid}")
                expected_cpu = (
                    (accumulated - previous_accumulated) * 100_000_000.0
                    / (observed_ns - previous_ns)
                )
            previous_by_identity[identity] = (accumulated, observed_ns)
            actual_cpu = finite_non_negative_number(process.get("cpu_percent"), "process cpu_percent", path)
            if not math.isclose(actual_cpu, expected_cpu, rel_tol=1e-12, abs_tol=1e-9):
                raise ValueError(
                    f"{path}: PID {pid} CPU {actual_cpu} does not match accumulated/raw-time {expected_cpu}"
                )
            expected_role = raw_process_role(root_pid, process)
            require_artifact_value(path, f"PID {pid} role", process.get("role"), expected_role)
            executable = process.get("executable")
            executable_bytes = process.get("executable_bytes")
            executable_sha = process.get("executable_sha256")
            if executable is not None and (not isinstance(executable, str) or not executable):
                raise ValueError(f"{path}: PID {pid} executable must be null or non-empty")
            if executable_sha is not None and (
                not isinstance(executable_sha, str) or re.fullmatch(r"[0-9a-f]{64}", executable_sha) is None
            ):
                raise ValueError(f"{path}: PID {pid} executable SHA-256 is invalid")
            if executable_bytes is not None:
                executable_bytes = non_negative_integer(
                    executable_bytes, "process executable bytes", path
                )
            if pid == root_pid:
                if not executable or not executable_bytes:
                    raise ValueError(f"{path}: root process has no executable identity")
                require_artifact_value(path, "root executable SHA-256", executable_sha, header.get("binary_sha256"))
            if expected_role == "mpv" and (
                not executable or not executable_bytes or executable_sha is None
            ):
                raise ValueError(f"{path}: measured mpv PID {pid} has no executable identity")
            if (executable is None) != (executable_bytes is None) or (
                executable is None
            ) != (executable_sha is None):
                raise ValueError(f"{path}: PID {pid} executable identity is partial")
            aggregate = computed_roles.setdefault(
                expected_role, {"processes": 0, "cpu_percent": 0.0, "rss_bytes": 0}
            )
            aggregate["processes"] = int(aggregate["processes"]) + 1
            aggregate["cpu_percent"] = float(aggregate["cpu_percent"]) + expected_cpu
            aggregate["rss_bytes"] = int(aggregate["rss_bytes"]) + rss
            if expected_role == "mpv":
                if expected_phase == "measure":
                    require_effective_mpv_cache_args(
                        path,
                        command,
                        expected_mpv_cache_args or {},
                        f"sample {index} mpv PID {pid}",
                    )
                identity_argv, endpoint = mpv_ipc_identity(command)
                sample_mpv.append(
                    {
                        "pid": pid,
                        "start_time_unix_s": start_time,
                        "input_ipc_server_argv": identity_argv,
                        "endpoint": endpoint,
                        "silent": silent_mpv_command(command),
                        "process": process,
                    }
                )
        if root_pid not in seen_pids:
            raise ValueError(f"{path}: sample {index} does not contain root PID {root_pid}")
        for descendant_pid in parent_by_pid:
            if descendant_pid == root_pid:
                continue
            current = descendant_pid
            visited: set[int] = set()
            while current != root_pid:
                if current in visited:
                    raise ValueError(
                        f"{path}: sample {index} PID {descendant_pid} has a parent cycle"
                    )
                visited.add(current)
                parent = parent_by_pid.get(current)
                if parent is None:
                    raise ValueError(
                        f"{path}: sample {index} PID {descendant_pid} has a missing parent"
                    )
                if parent == current:
                    raise ValueError(
                        f"{path}: sample {index} PID {current} is its own parent"
                    )
                if parent not in parent_by_pid:
                    raise ValueError(
                        f"{path}: sample {index} PID {descendant_pid} escapes the sampled root"
                    )
                current = parent
        tree = {
            "processes": sum(int(item["processes"]) for item in computed_roles.values()),
            "cpu_percent": sum(float(item["cpu_percent"]) for item in computed_roles.values()),
            "rss_bytes": sum(int(item["rss_bytes"]) for item in computed_roles.values()),
        }
        expected_roles = {**computed_roles, "tree": tree}
        roles = record.get("roles")
        if not isinstance(roles, dict) or set(roles) != set(expected_roles):
            raise ValueError(f"{path}: sample {index} role inventory does not match raw processes")
        for role_name, expected_values in expected_roles.items():
            values = roles[role_name]
            if not isinstance(values, dict) or set(values) != {"processes", "cpu_percent", "rss_bytes"}:
                raise ValueError(f"{path}: sample {index} role {role_name} has invalid schema")
            require_artifact_value(path, f"sample {index} {role_name} processes", values["processes"], expected_values["processes"])
            require_artifact_value(path, f"sample {index} {role_name} RSS", values["rss_bytes"], expected_values["rss_bytes"])
            actual_role_cpu = finite_non_negative_number(values["cpu_percent"], f"sample {index} {role_name} CPU", path)
            if not math.isclose(actual_role_cpu, float(expected_values["cpu_percent"]), rel_tol=1e-12, abs_tol=1e-9):
                raise ValueError(f"{path}: sample {index} {role_name} CPU does not match raw processes")
        present = bool(sample_mpv)
        all_silent = present and all(item["silent"] for item in sample_mpv)
        all_identified = present and all(item["input_ipc_server_argv"] is not None for item in sample_mpv)
        require_artifact_value(path, f"sample {index} mpv_present", record.get("mpv_present"), present)
        require_artifact_value(path, f"sample {index} mpv_all_silent", record.get("mpv_all_silent_this_sample"), all_silent)
        if sample_mpv and all_identified:
            last_mpv_identities = [
                {
                    "pid": item["pid"],
                    "start_time_unix_s": item["start_time_unix_s"],
                    "executable": item["process"]["executable"],
                    "executable_bytes": item["process"]["executable_bytes"],
                    "executable_sha256": item["process"]["executable_sha256"],
                    "input_ipc_server_argv": item["input_ipc_server_argv"],
                }
                for item in sample_mpv
            ]
        if expected_phase == "measure":
            proof["samples"] += 1
            proof["samples_with_mpv"] += int(present)
            proof["samples_all_silent"] += int(all_silent)
            proof["samples_all_cleanup_identified"] += int(all_identified)
            raw_elapsed = observed_ns // 1_000_000
            elapsed.append(raw_elapsed)
            for role_name, values in expected_roles.items():
                role_points.setdefault(role_name, []).append(
                    (
                        float(values["cpu_percent"]),
                        int(values["rss_bytes"]),
                        expected_cpu_overlap_ns,
                    )
                )
        recomputed_samples.append({"record": record, "mpv": sample_mpv, "roles": expected_roles})
    if elapsed != sorted(elapsed) or len(elapsed) != len(set(elapsed)):
        raise ValueError(f"{path}: measured elapsed_ms values must be strictly increasing")
    if len(elapsed) >= 2:
        expected_span = (len(elapsed) - 1) * interval_ms
        actual_span = elapsed[-1] - elapsed[0]
        schedule_tolerance = max(50, interval_ms // 4)
        if abs(actual_span - expected_span) > schedule_tolerance:
            raise ValueError(
                f"{path}: measured sampling span {actual_span}ms does not match "
                f"{expected_span}ms within {schedule_tolerance}ms"
            )
    if elapsed[0] < max(0, warmup_ms - interval_ms) or elapsed[0] > warmup_ms + interval_ms:
        raise ValueError(f"{path}: measured phase starts outside one sampling interval of warmup")
    if previous_observed_ns < cpu_window_end_ns:
        raise ValueError(f"{path}: final raw CPU interval does not reach the declared window end")
    if elapsed[-1] > (cpu_window_end_ns // 1_000_000) + interval_ms:
        raise ValueError(f"{path}: final measured sample is more than one interval late")
    if total_cpu_overlap_ns != cpu_window_duration_ns:
        raise ValueError(
            f"{path}: raw CPU intervals cover {total_cpu_overlap_ns}ns, "
            f"expected the full {cpu_window_duration_ns}ns measurement window"
        )

    summary_roles = summary.get("roles")
    if not isinstance(summary_roles, dict) or set(summary_roles) != set(role_points):
        raise ValueError(f"{path}: summary roles do not match measured raw roles")
    for role, points in role_points.items():
        values = summary_roles[role]
        if not isinstance(values, dict):
            raise ValueError(f"{path}: summary role {role} must be an object")
        expected_cpu = (
            sum(cpu * overlap_ns for cpu, _rss, overlap_ns in points)
            / cpu_window_duration_ns
        )
        expected_mean_rss = sum(rss for _cpu, rss, _overlap_ns in points) // len(points)
        expected_peak_rss = max(rss for _cpu, rss, _overlap_ns in points)
        require_artifact_value(path, f"{role} summary samples", values.get("samples"), len(points))
        actual_cpu = finite_non_negative_number(
            values.get("mean_cpu_percent"), f"summary {role}.mean_cpu_percent", path
        )
        if not math.isclose(actual_cpu, expected_cpu, rel_tol=1e-12, abs_tol=1e-9):
            raise ValueError(
                f"{path}: {role} mean CPU {actual_cpu} does not match raw {expected_cpu}"
            )
        require_artifact_value(
            path, f"{role} mean RSS", values.get("mean_rss_bytes"), expected_mean_rss
        )
        require_artifact_value(
            path, f"{role} peak RSS", values.get("peak_rss_bytes"), expected_peak_rss
        )
    proven = proof["samples"] > 0 and proof["samples_with_mpv"] == proof["samples"] and proof["samples_all_silent"] == proof["samples"]
    cleanup_proven = proof["samples"] > 0 and proof["samples_all_cleanup_identified"] == proof["samples"]
    require_artifact_value(path, "summary root PID", summary.get("root_pid"), root_pid)
    require_artifact_value(path, "summary measured mpv proof", summary.get("measured_mpv_proof"), proof)
    require_artifact_value(path, "summary silent mpv proof", summary.get("silent_mpv_proven"), proven)
    require_artifact_value(path, "summary last mpv identity", summary.get("last_observed_mpv"), last_mpv_identities)
    if require_mpv and (not proven or not cleanup_proven):
        raise ValueError(f"{path}: raw measured samples do not prove silent, cleanup-identified mpv")
    return {
        "root_pid": root_pid,
        "samples": recomputed_samples,
        "measured": [item for item in recomputed_samples if item["record"]["phase"] == "measure"],
        "last_mpv_identities": last_mpv_identities,
    }


def sample_tree_topology_self_test() -> None:
    header = {
        "root_pid": 100,
        "binary_sha256": "ab" * 32,
        "warmup_ms": 1_000,
        "duration_ms": 2_000,
        "cpu_accounting": CPU_ACCOUNTING_METHOD,
        "cpu_window_start_ns": 1_000_000_000,
        "cpu_window_end_ns": 3_000_000_000,
        "interval_ms": 1_000,
    }

    def process(
        pid: int,
        parent_pid: int | None,
        role: str,
        name: str,
        start_time: int,
        accumulated_cpu_ms: int,
        cpu_percent: float,
        rss_bytes: int,
        root: bool = False,
    ) -> dict[str, Any]:
        return {
            "pid": pid,
            "parent_pid": parent_pid,
            "role": role,
            "name": name,
            "start_time_unix_s": start_time,
            "accumulated_cpu_ms": accumulated_cpu_ms,
            "cpu_percent": cpu_percent,
            "rss_bytes": rss_bytes,
            "command": [f"/tmp/{name}"],
            "executable": "/tmp/ytt" if root else None,
            "executable_bytes": 1 if root else None,
            "executable_sha256": "ab" * 32 if root else None,
        }

    def sample(
        elapsed_ms: int,
        cpu_overlap_ms: int,
        root_values: tuple[int, float, int],
        first_values: tuple[int, float, int],
        second_values: tuple[int, float, int],
    ) -> dict[str, Any]:
        root_accumulated, root_cpu, root_rss = root_values
        first_accumulated, first_cpu, first_rss = first_values
        second_accumulated, second_cpu, second_rss = second_values
        other_cpu = first_cpu + second_cpu
        other_rss = first_rss + second_rss
        tree_cpu = root_cpu + other_cpu
        tree_rss = root_rss + other_rss
        return {
            "schema": "ytt.tui-perf.samples.v1",
            "kind": "sample",
            "elapsed_ms": elapsed_ms,
            "observed_elapsed_ns": elapsed_ms * 1_000_000,
            "cpu_interval_overlap_ns": cpu_overlap_ms * 1_000_000,
            "phase": "warmup" if elapsed_ms < 1_000 else "measure",
            "mpv_present": False,
            "mpv_all_silent_this_sample": False,
            "roles": {
                "ytt": {
                    "processes": 1,
                    "cpu_percent": root_cpu,
                    "rss_bytes": root_rss,
                },
                "other": {
                    "processes": 2,
                    "cpu_percent": other_cpu,
                    "rss_bytes": other_rss,
                },
                "tree": {
                    "processes": 3,
                    "cpu_percent": tree_cpu,
                    "rss_bytes": tree_rss,
                },
            },
            "processes": [
                process(
                    100,
                    1,
                    "ytt",
                    "ytt",
                    50,
                    root_accumulated,
                    root_cpu,
                    root_rss,
                    True,
                ),
                process(
                    200,
                    100,
                    "other",
                    "helper-a",
                    51,
                    first_accumulated,
                    first_cpu,
                    first_rss,
                ),
                process(
                    201,
                    100,
                    "other",
                    "helper-b",
                    52,
                    second_accumulated,
                    second_cpu,
                    second_rss,
                ),
            ],
        }

    records = [
        sample(0, 0, (0, 0.0, 50), (0, 0.0, 10), (0, 0.0, 15)),
        sample(1_000, 0, (100, 10.0, 100), (20, 2.0, 20), (30, 3.0, 25)),
        sample(2_000, 1_000, (300, 20.0, 200), (60, 4.0, 30), (80, 5.0, 35)),
        sample(3_000, 1_000, (600, 30.0, 300), (120, 6.0, 40), (150, 7.0, 45)),
    ]
    summary = {
        "cpu_accounting": CPU_ACCOUNTING_METHOD,
        "cpu_window_start_ns": 1_000_000_000,
        "cpu_window_end_ns": 3_000_000_000,
        "roles": {
            "ytt": {
                "samples": 3,
                "mean_cpu_percent": 25.0,
                "mean_rss_bytes": 200,
                "peak_rss_bytes": 300,
            },
            "other": {
                "samples": 3,
                "mean_cpu_percent": 11.0,
                "mean_rss_bytes": 65,
                "peak_rss_bytes": 85,
            },
            "tree": {
                "samples": 3,
                "mean_cpu_percent": 36.0,
                "mean_rss_bytes": 265,
                "peak_rss_bytes": 385,
            },
        },
        "root_pid": 100,
        "silent_mpv_proven": False,
        "measured_mpv_proof": {
            "samples": 3,
            "samples_with_mpv": 0,
            "samples_all_silent": 0,
            "samples_all_cleanup_identified": 0,
        },
        "last_observed_mpv": [],
    }
    path = Path("<sample-tree-self-test>")
    validate_measured_samples(path, header, summary, records, False)

    def expect_parent_rejected(
        label: str, first_parent: int | None, second_parent: int | None = 100
    ) -> None:
        tampered = json.loads(json.dumps(records))
        for record in tampered:
            by_pid = {item["pid"]: item for item in record["processes"]}
            by_pid[200]["parent_pid"] = first_parent
            by_pid[201]["parent_pid"] = second_parent
        try:
            validate_measured_samples(path, header, summary, tampered, False)
        except ValueError:
            pass
        else:
            raise AssertionError(f"sample parent topology tampering was accepted: {label}")

    # Roles, CPU, RSS, and summary totals remain exactly recomputed; only ancestry is hostile.
    expect_parent_rejected("A-B cycle", 201, 200)
    expect_parent_rejected("self parent", 200)
    expect_parent_rejected("missing parent", None)
    expect_parent_rejected("escaped parent", 999)


def validate_process_directory(
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_document: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
    seed_context: tuple[dict[str, Any], Path] | None,
    run_contract: dict[str, Any],
    additional_metric_files: list[Path],
) -> list[Path]:
    samples_path = path / "samples.ndjson"
    if not samples_path.is_file():
        raise ValueError(f"{path}: missing samples.ndjson")
    samples = read_ndjson(samples_path)
    headers = [record for record in samples if record.get("kind") == "header"]
    summaries = [record for record in samples if record.get("kind") == "summary"]
    measured = [
        record
        for record in samples
        if record.get("kind") == "sample" and record.get("phase") == "measure"
    ]
    all_sample_records = [record for record in samples if record.get("kind") == "sample"]
    unexpected_sample_kinds = sorted(
        {
            str(record.get("kind"))
            for record in samples
            if record.get("kind") not in {"header", "sample", "summary", "error"}
        }
    )
    if unexpected_sample_kinds:
        raise ValueError(
            f"{samples_path}: unexpected sampler record kinds {unexpected_sample_kinds}"
        )
    if len(headers) != 1 or len(summaries) != 1 or not measured:
        raise ValueError(f"{samples_path}: incomplete sampler header, summary, or measurements")
    header = headers[0]
    summary = summaries[0]
    require_artifact_value(samples_path, "schema", header.get("schema"), "ytt.tui-perf.samples.v1")
    require_artifact_value(samples_path, "scenario SHA-256", header.get("scenario_sha256"), scenario_hash)
    require_artifact_value(samples_path, "run ID", header.get("run_id"), run_contract["run_id"])
    require_artifact_value(
        samples_path, "summary run ID", summary.get("run_id"), run_contract["run_id"]
    )
    validate_sampler_terminal_geometry(samples_path, header, summary, run_contract)
    sampler_started_unix_ns = non_negative_integer(
        header.get("observation_started_unix_ns"), "sampler observation start", samples_path
    )
    sampler_finished_unix_ns = non_negative_integer(
        summary.get("observation_finished_unix_ns"), "sampler observation finish", samples_path
    )
    if not run_contract["started_unix_ns"] <= sampler_started_unix_ns < sampler_finished_unix_ns <= run_contract["finished_unix_ns"]:
        raise ValueError(f"{samples_path}: sampler producer interval escapes its run contract")
    require_artifact_value(samples_path, "OS", normalized_os(header.get("os")), host_os)
    require_artifact_value(
        samples_path,
        "executed binary SHA-256",
        header.get("binary_sha256"),
        manifest["binaries"][f"{role}_ytt"]["sha256"],
    )
    require_artifact_value(
        samples_path,
        "sampler producer SHA-256",
        header.get("producer_binary_sha256"),
        manifest["binaries"]["sampler"]["sha256"],
    )
    require_artifact_value(
        samples_path,
        "warmup_ms",
        header.get("warmup_ms"),
        int(float(scenario["warmup_s"]) * 1000),
    )
    require_artifact_value(
        samples_path,
        "duration_ms",
        header.get("duration_ms"),
        int(float(scenario["sample_s"]) * 1000),
    )
    expected_cpu_window_start_ns = int(round(float(scenario["warmup_s"]) * 1_000_000_000))
    expected_cpu_window_end_ns = expected_cpu_window_start_ns + int(
        round(float(scenario["sample_s"]) * 1_000_000_000)
    )
    require_artifact_value(
        samples_path,
        "CPU accounting method",
        header.get("cpu_accounting"),
        CPU_ACCOUNTING_METHOD,
    )
    require_artifact_value(
        samples_path,
        "CPU window start",
        header.get("cpu_window_start_ns"),
        expected_cpu_window_start_ns,
    )
    require_artifact_value(
        samples_path,
        "CPU window end",
        header.get("cpu_window_end_ns"),
        expected_cpu_window_end_ns,
    )
    require_artifact_value(
        samples_path,
        "interval_ms",
        header.get("interval_ms"),
        scenario_document["sampling"]["interval_ms"],
    )
    require_artifact_value(
        samples_path,
        "silent mpv policy",
        header.get("require_silent_mpv"),
        scenario["requires_mpv"],
    )
    require_artifact_value(
        samples_path,
        "controller barrier policy",
        header.get("controller_barrier_required"),
        bool(scenario["controller"]),
    )
    require_artifact_value(
        samples_path,
        "YTM_PERF launch instrumentation",
        header.get("child_ytm_perf_enabled"),
        False,
    )
    if any(record.get("kind") == "error" for record in samples):
        raise ValueError(f"{samples_path}: sampler recorded an error")
    sample_validation = validate_measured_samples(
        samples_path,
        header,
        summary,
        all_sample_records,
        bool(scenario["requires_mpv"]),
        scenario.get("expected_effective_mpv_cache_args"),
    )

    identity_path = path / "process-identity.json"
    if not identity_path.is_file():
        raise ValueError(f"{path}: missing process-identity.json")
    live_identity = load_json_object(identity_path)
    live_producer, live_owner, live_mpv, live_descendants = validated_live_identity(
        live_identity, identity_path
    )
    require_artifact_value(
        identity_path, "run ID", live_identity.get("run_id"), run_contract["run_id"]
    )
    require_artifact_value(identity_path, "cleanup state", live_identity.get("state"), "cleaned")
    require_artifact_value(identity_path, "cleanup proof", live_identity.get("cleanup_proven"), True)
    first_root = next(
        process
        for process in sample_validation["measured"][0]["record"]["processes"]
        if process["pid"] == sample_validation["root_pid"]
    )
    expected_owner = {
        field: first_root[field]
        for field in (
            "pid", "start_time_unix_s", "executable", "executable_bytes", "executable_sha256"
        )
    }
    if not isinstance(live_owner, dict):
        raise ValueError(f"{identity_path}: cleaned measured run has no complete owner identity")
    for field, expected in expected_owner.items():
        require_artifact_value(
            identity_path, f"owner identity {field}", live_owner.get(field), expected
        )
    require_artifact_value(
        identity_path,
        "sampler producer executable SHA-256",
        live_producer.get("executable_sha256"),
        header.get("producer_binary_sha256"),
    )
    require_artifact_value(
        identity_path,
        "mpv identity",
        live_mpv,
        sample_validation["last_mpv_identities"],
    )
    last_processes = sample_validation["samples"][-1]["record"]["processes"]
    expected_descendants = [
        {
            "pid": process["pid"],
            "start_time_unix_s": process["start_time_unix_s"],
            "executable": process["executable"],
            "executable_bytes": process["executable_bytes"],
            "executable_sha256": process["executable_sha256"],
            "role": process["role"],
            "command": process["command"],
        }
        for process in last_processes
        if process["pid"] != sample_validation["root_pid"]
    ]
    require_artifact_value(
        identity_path, "recursive descendant identity", live_descendants, expected_descendants
    )

    launch_policy_path = path / "launch-policy.json"
    if not launch_policy_path.is_file():
        raise ValueError(f"{path}: missing launch-policy.json")
    _launch_policy, launch_artifacts = validate_launch_policy(launch_policy_path, path)
    artifacts = [samples_path, identity_path, *launch_artifacts, *additional_metric_files]
    if scenario["controller"]:
        control_path = path / "control.ndjson"
        if not control_path.is_file():
            raise ValueError(f"{path}: missing control.ndjson")
        control = read_ndjson(control_path)
        unexpected_control_kinds = sorted(
            {
                str(record.get("kind"))
                for record in control
                if record.get("kind")
                not in {"header", "mpv_event", "operation", "summary", "error"}
            }
        )
        if unexpected_control_kinds:
            raise ValueError(
                f"{control_path}: unexpected controller record kinds "
                f"{unexpected_control_kinds}"
            )
        if any(record.get("kind") == "error" for record in control):
            raise ValueError(f"{control_path}: controller recorded an error")
        control_headers = [record for record in control if record.get("kind") == "header"]
        control_summaries = [record for record in control if record.get("kind") == "summary"]
        if len(control_headers) != 1 or len(control_summaries) != 1:
            raise ValueError(f"{control_path}: incomplete controller header or summary")
        control_header = control_headers[0]
        require_artifact_value(control_path, "schema", control_header.get("schema"), "ytt.tui-perf.control.v1")
        require_artifact_value(control_path, "scenario SHA-256", control_header.get("scenario_sha256"), scenario_hash)
        require_artifact_value(control_path, "run ID", control_header.get("run_id"), run_contract["run_id"])
        require_artifact_value(control_path, "OS", normalized_os(control_header.get("os")), host_os)
        require_artifact_value(
            control_path,
            "controller producer SHA-256",
            control_header.get("producer_binary_sha256"),
            manifest["binaries"]["controller"]["sha256"],
        )
        expected_observe_ns = int(
            (float(scenario["warmup_s"]) + float(scenario["sample_s"]))
            * 1_000_000_000
        )
        require_artifact_value(control_path, "confirmed subscriptions", control_header.get("subscriptions_confirmed"), True)
        require_artifact_value(control_path, "observation duration", control_header.get("observe_ns"), expected_observe_ns)
        require_artifact_value(
            control_path,
            "buffering cutoff",
            control_header.get("buffering_cutoff_ns"),
            expected_observe_ns,
        )
        close_grace_ns = non_negative_integer(control_header.get("close_grace_ns"), "close_grace_ns", control_path)
        if close_grace_ns == 0:
            raise ValueError(f"{control_path}: close_grace_ns must be positive")
        sampler_started_ns = sampler_started_unix_ns
        controller_started_ns = non_negative_integer(
            control_header.get("observation_started_unix_ns"), "controller observation start", control_path
        )
        if sampler_started_ns < controller_started_ns or sampler_started_ns - controller_started_ns > 1_000_000_000:
            raise ValueError(f"{control_path}: sampler/controller barrier starts are not aligned")

        measured_validation = sample_validation["measured"]
        owner_rows = []
        mpv_rows = []
        for sample_index, sample_item in enumerate(measured_validation):
            roots = [
                process
                for process in sample_item["record"]["processes"]
                if process["pid"] == sample_validation["root_pid"]
            ]
            if len(roots) != 1:
                raise ValueError(f"{samples_path}: measured sample {sample_index} has invalid owner inventory")
            owner_rows.append(roots[0])
            if len(sample_item["mpv"]) != 1:
                raise ValueError(f"{samples_path}: measured sample {sample_index} must contain exactly one mpv")
            mpv_rows.append(sample_item["mpv"][0])
        owner_identity = {
            (row["pid"], row["start_time_unix_s"], row["executable"], row["executable_bytes"], row["executable_sha256"])
            for row in owner_rows
        }
        mpv_identity = {
            (
                item["pid"], item["start_time_unix_s"], item["process"]["executable"],
                item["process"]["executable_bytes"], item["process"]["executable_sha256"], item["endpoint"],
            )
            for item in mpv_rows
        }
        if len(owner_identity) != 1 or len(mpv_identity) != 1:
            raise ValueError(f"{samples_path}: owner/mpv executable identity changed during measurement")
        owner_pid, owner_start, owner_exe, owner_bytes, owner_sha = next(iter(owner_identity))
        mpv_pid, mpv_start, mpv_exe, mpv_bytes, mpv_sha, mpv_endpoint = next(iter(mpv_identity))
        for label, actual, expected in (
            ("owner PID", control_header.get("owner_pid"), owner_pid),
            ("owner start", control_header.get("owner_start_time_unix_s"), owner_start),
            ("owner executable", control_header.get("owner_executable"), owner_exe),
            ("owner executable bytes", control_header.get("owner_executable_bytes"), owner_bytes),
            ("owner executable SHA-256", control_header.get("owner_executable_sha256"), owner_sha),
            ("mpv PID", control_header.get("mpv_pid"), mpv_pid),
            ("mpv start", control_header.get("mpv_start_time_unix_s"), mpv_start),
            ("mpv executable", control_header.get("mpv_executable"), mpv_exe),
            ("mpv executable bytes", control_header.get("mpv_executable_bytes"), mpv_bytes),
            ("mpv executable SHA-256", control_header.get("mpv_executable_sha256"), mpv_sha),
            ("mpv endpoint", control_header.get("mpv_endpoint"), mpv_endpoint),
        ):
            require_artifact_value(control_path, label, actual, expected)

        ready_path = path / "controller-ready.json"
        if not ready_path.is_file():
            raise ValueError(f"{path}: missing controller-ready.json")
        ready = load_json_object(ready_path)
        require_artifact_value(ready_path, "schema", ready.get("schema"), "ytt.tui-perf.controller-ready.v1")
        require_artifact_value(ready_path, "run ID", ready.get("run_id"), run_contract["run_id"])
        require_artifact_value(ready_path, "scenario SHA-256", ready.get("scenario_sha256"), scenario_hash)
        require_artifact_value(ready_path, "owner PID", ready.get("owner_pid"), owner_pid)
        require_artifact_value(ready_path, "owner start", ready.get("owner_start_time_unix_s"), owner_start)
        require_artifact_value(ready_path, "mpv PID", ready.get("mpv_pid"), mpv_pid)
        require_artifact_value(ready_path, "mpv start", ready.get("mpv_start_time_unix_s"), mpv_start)
        require_artifact_value(ready_path, "mpv endpoint", ready.get("mpv_endpoint"), mpv_endpoint)
        require_artifact_value(ready_path, "confirmed subscriptions", ready.get("subscriptions_confirmed"), True)
        require_artifact_value(ready_path, "observation start", ready.get("observation_started_unix_ns"), controller_started_ns)

        control_summary = control_summaries[0]
        require_artifact_value(
            control_path, "summary run ID", control_summary.get("run_id"), run_contract["run_id"]
        )
        controller_finished_ns = non_negative_integer(
            control_summary.get("observation_finished_unix_ns"),
            "controller observation finish",
            control_path,
        )
        if not run_contract["started_unix_ns"] <= controller_started_ns < controller_finished_ns <= run_contract["finished_unix_ns"]:
            raise ValueError(f"{control_path}: controller producer interval escapes its run contract")
        validate_control_buffering(control_path, control, control_summary)
        validate_control_time_pos_summary(control_path, control, control_summary)
        validate_steady_playback_progress(
            control_path, control, control_summary, scenario
        )
        summary_elapsed_ns = non_negative_integer(control_summary.get("elapsed_ns"), "summary elapsed_ns", control_path)
        require_artifact_value(control_path, "expected observation coverage", control_summary.get("expected_observation_ns"), expected_observe_ns)
        require_artifact_value(control_path, "actual observation coverage", control_summary.get("actual_coverage_ns"), expected_observe_ns)
        require_artifact_value(
            control_path,
            "summary buffering cutoff",
            control_summary.get("buffering_cutoff_ns"),
            expected_observe_ns,
        )
        require_artifact_value(control_path, "terminal kind", control_summary.get("terminal_kind"), "clean_eof")
        terminal_ns = non_negative_integer(control_summary.get("terminal_observed_ns"), "terminal_observed_ns", control_path)
        if terminal_ns < expected_observe_ns or terminal_ns > summary_elapsed_ns:
            raise ValueError(f"{control_path}: clean EOF is outside the declared coverage")
        if summary_elapsed_ns > expected_observe_ns + close_grace_ns + 1_000_000_000:
            raise ValueError(f"{control_path}: controller exceeded observation plus close grace")
        event_ns = [
            non_negative_integer(record.get("elapsed_ns"), "mpv event elapsed_ns", control_path)
            for record in control
            if record.get("kind") == "mpv_event"
        ]
        require_artifact_value(control_path, "first event boundary", control_summary.get("first_event_ns"), event_ns[0] if event_ns else None)
        require_artifact_value(control_path, "last event boundary", control_summary.get("last_event_ns"), event_ns[-1] if event_ns else None)
        require_artifact_value(
            control_path,
            "pause policy",
            control_header.get("pause_policy"),
            scenario["pause_policy"],
        )
        expected_hold = scenario["pause_hold_ms"] if scenario["pause_policy"] == "pause-resume" else None
        require_artifact_value(control_path, "pause hold", control_header.get("pause_hold_ms"), expected_hold)
        require_artifact_value(
            control_path,
            "observation end",
            control_summaries[0].get("observation_end"),
            "mpv_ipc_closed",
        )
        validate_control_operations(
            control_path, control, summary_elapsed_ns, scenario
        )
        artifacts.extend((control_path, ready_path))

    if scenario["requires_mpv"]:
        if seed_context is None:
            raise ValueError(f"{path}: playback run has no validated seed contract")
        seed_manifest, seed_snapshot = seed_context
        materialize_path = path / "materialize.json"
        http_path = path / "http-ready.json"
        http_requests_path = path / "http-requests.ndjson"
        for support in (materialize_path, http_path, http_requests_path):
            if not support.is_file():
                raise ValueError(f"{path}: missing {support.name}")
        materialize = load_json_object(materialize_path)
        require_artifact_value(materialize_path, "schema", materialize.get("schema"), "ytt.tui-perf.materialize.v1")
        require_artifact_value(materialize_path, "seed label", materialize.get("seed_label"), role)
        require_artifact_value(
            materialize_path,
            "playback target",
            materialize.get("playback_target_mode"),
            "local_m3u_indirection",
        )
        require_artifact_value(materialize_path, "external DNS", materialize.get("external_dns_required"), False)
        require_artifact_value(
            materialize_path,
            "seed tree SHA-256",
            materialize.get("seed_tree_sha256"),
            seed_manifest["snapshot_tree_sha256"],
        )
        require_artifact_value(
            materialize_path,
            "seed cache policy",
            materialize.get("seed_cache_policy"),
            seed_manifest["cache_policy"],
        )
        require_artifact_value(
            materialize_path,
            "materializer SHA-256",
            materialize.get("materializer_sha256"),
            manifest["orchestrator"]["sha256"],
        )
        http = load_json_object(http_path)
        require_artifact_value(http_path, "schema", http.get("schema"), "ytt.tui-perf.http.v1")
        require_artifact_value(http_path, "loopback binding", http.get("bind_is_loopback"), True)
        require_artifact_value(http_path, "playback target", http.get("playback_target_mode"), "local_m3u_indirection")
        require_artifact_value(http_path, "external DNS", http.get("external_dns_required"), False)
        require_artifact_value(http_path, "run ID", http.get("run_id"), run_contract["run_id"])
        http_started_unix_ns = non_negative_integer(
            http.get("started_unix_ns"), "HTTP server wall start", http_path
        )
        http_started_monotonic_ns = non_negative_integer(
            http.get("started_monotonic_ns"), "HTTP server monotonic start", http_path
        )
        if not (
            run_contract["started_unix_ns"] <= http_started_unix_ns <= run_contract["finished_unix_ns"]
            and run_contract["started_monotonic_ns"]
            <= http_started_monotonic_ns
            <= run_contract["finished_monotonic_ns"]
        ):
            raise ValueError(f"{http_path}: HTTP server start escapes its run contract")
        parsed_http = urlsplit(str(http.get("url", "")))
        try:
            http_ip = ipaddress.ip_address(parsed_http.hostname or "")
        except ValueError as error:
            raise ValueError(f"{http_path}: URL host is not an IP literal") from error
        if parsed_http.scheme != "http" or not http_ip.is_loopback:
            raise ValueError(f"{http_path}: URL must use an HTTP loopback endpoint")
        require_artifact_value(http_path, "URL host", str(http_ip), str(http.get("host")))
        require_artifact_value(
            materialize_path, "fixture URL", materialize.get("fixture_url"), http.get("url")
        )
        require_artifact_value(
            materialize_path, "fixture host", materialize.get("fixture_host"), str(http.get("host"))
        )
        profile = scenario_document["traffic_profiles"][scenario["traffic_profile"]]
        for field in ("throttle_bps", "outage_every_bytes", "outage_ms", "disconnect_every_bytes"):
            require_artifact_value(http_path, field, http.get(field), profile[field])
        require_artifact_value(
            http_path,
            "request log path",
            Path(str(http.get("request_log", ""))).resolve(),
            http_requests_path.resolve(),
        )
        if not isinstance(http.get("run_id"), str) or not http["run_id"]:
            raise ValueError(f"{http_path}: run_id is missing")
        validate_http_request_log(http_requests_path, http, profile, run_contract)
        changed = materialize.get("changed")
        if not isinstance(changed, list) or not changed or not all(
            isinstance(relative, str) and relative for relative in changed
        ):
            raise ValueError(f"{materialize_path}: changed must be a non-empty path list")
        input_snapshot = path / "materialized-inputs"
        if materialize.get("input_snapshot") != input_snapshot.name or not input_snapshot.is_dir():
            raise ValueError(f"{materialize_path}: materialized input snapshot is missing")
        overlay_digest, overlay_inventory = overlay_tree_identity(
            seed_snapshot, input_snapshot, changed
        )
        require_artifact_value(
            materialize_path,
            "materialized tree SHA-256",
            materialize.get("materialized_tree_sha256"),
            overlay_digest,
        )
        require_artifact_value(
            materialize_path,
            "materialized file inventory",
            materialize.get("materialized_files"),
            overlay_inventory,
        )
        require_artifact_value(
            materialize_path,
            "input snapshot file inventory",
            materialize.get("input_snapshot_files"),
            tree_file_inventory(input_snapshot),
        )
        playlist_relative = materialize.get("playlist")
        if not isinstance(playlist_relative, str) or playlist_relative not in changed:
            raise ValueError(f"{materialize_path}: playlist is not a changed relative input")
        playlist = (input_snapshot / playlist_relative).resolve()
        try:
            playlist.relative_to(input_snapshot.resolve())
        except ValueError as error:
            raise ValueError(f"{materialize_path}: playlist escapes input snapshot") from error
        expected_playlist = (
            "#EXTM3U\n#EXTINF:-1,ytt deterministic performance fixture\n"
            f"{http.get('url')}\n"
        )
        require_artifact_value(
            playlist, "playlist content", playlist.read_text(encoding="utf-8"), expected_playlist
        )
        require_artifact_value(
            materialize_path,
            "playlist SHA-256",
            materialize.get("playlist_sha256"),
            sha256_file(playlist),
        )
        require_artifact_value(
            materialize_path,
            "seed active playlist contract",
            materialize.get("seed_active_playlist_contract"),
            seed_manifest["active_playlist_contract"],
        )
        materialized_contract = materialize.get("materialized_active_playlist_contract")
        if not isinstance(materialized_contract, dict):
            raise ValueError(f"{materialize_path}: materialized active playlist contract is missing")
        require_artifact_value(
            materialize_path,
            "materialized active playlist",
            validate_materialized_active_session_playlist(
                input_snapshot, str(materialized_contract.get("local_path", ""))
            ),
            materialized_contract,
        )
        require_artifact_value(
            materialize_path,
            "materialized active local_path",
            materialized_contract.get("local_path"),
            str((path / "home" / playlist_relative).resolve()),
        )
        artifacts.extend((materialize_path, http_path, http_requests_path))
        artifacts.extend(
            sorted(item for item in input_snapshot.rglob("*") if item.is_file())
        )
    expected_metric_files = {
        artifact.resolve()
        for artifact in artifacts
        if artifact.parent.resolve() == path.resolve()
        and artifact.suffix in {".json", ".ndjson"}
    }
    actual_metric_files = {
        item.resolve()
        for item in path.iterdir()
        if item.is_file() and item.suffix in {".json", ".ndjson"}
    }
    if actual_metric_files != expected_metric_files:
        unexpected = sorted(str(item) for item in actual_metric_files - expected_metric_files)
        missing = sorted(str(item) for item in expected_metric_files - actual_metric_files)
        raise ValueError(
            f"{path}: metric input inventory mismatch; unexpected={unexpected}, missing={missing}"
        )
    return artifacts


def validate_sampler_terminal_geometry(
    path: Path,
    header: dict[str, Any],
    summary: dict[str, Any],
    run_contract: dict[str, Any],
) -> None:
    expected = run_contract.get("terminal_geometry")
    if (
        not isinstance(expected, list)
        or len(expected) != 2
        or any(
            not isinstance(value, int) or isinstance(value, bool) or value <= 0
            for value in expected
        )
    ):
        raise ValueError(f"{path}: process run contract terminal geometry is malformed")
    require_artifact_value(
        path, "sampler terminal geometry", header.get("terminal_geometry"), expected
    )
    require_artifact_value(
        path,
        "sampler summary terminal geometry",
        summary.get("terminal_geometry"),
        expected,
    )


def validate_run_contract(
    path: Path,
    *,
    scenario: dict[str, Any],
    scenario_hash: str,
    host_identity: dict[str, str],
    kind: str,
    role: str,
    pair_index: int | None,
    repeat_index: int | None,
    geometry_index: int | None,
) -> dict[str, Any]:
    contract = load_json_object(path)
    expected_fields = {
        "schema",
        "state",
        "run_id",
        "scenario",
        "scenario_sha256",
        "kind",
        "role",
        "pair_index",
        "pair_order",
        "within_pair_ordinal",
        "repeat_index",
        "geometry",
        "geometry_index",
        "terminal_geometry",
        "started_unix_ns",
        "started_monotonic_ns",
        "finished_unix_ns",
        "finished_monotonic_ns",
        "duration_ns",
        "monotonic_clock",
        "host",
    }
    if set(contract) != expected_fields:
        raise ValueError(
            f"{path}: run contract fields are {sorted(contract)}, expected {sorted(expected_fields)}"
        )
    require_artifact_value(path, "schema", contract["schema"], RUN_CONTRACT_SCHEMA)
    require_artifact_value(path, "state", contract["state"], "finished")
    require_artifact_value(path, "scenario", contract["scenario"], scenario["id"])
    require_artifact_value(path, "scenario SHA-256", contract["scenario_sha256"], scenario_hash)
    require_artifact_value(path, "kind", contract["kind"], kind)
    require_artifact_value(path, "role", contract["role"], role)
    require_artifact_value(path, "pair index", contract["pair_index"], pair_index)
    require_artifact_value(path, "repeat index", contract["repeat_index"], repeat_index)
    require_artifact_value(path, "geometry", contract["geometry"], scenario["geometry"])
    require_artifact_value(path, "geometry index", contract["geometry_index"], geometry_index)
    expected_terminal_geometry = None
    if geometry_index is not None:
        expected_terminal_geometry = scenario["geometry"][geometry_index]
    elif scenario["id"] != "render_and_interaction" and len(scenario["geometry"]) == 1:
        expected_terminal_geometry = scenario["geometry"][0]
    require_artifact_value(
        path,
        "terminal geometry",
        contract["terminal_geometry"],
        expected_terminal_geometry,
    )
    run_id = contract["run_id"]
    if not isinstance(run_id, str) or not re.fullmatch(r"[^:]+:[^:]+:[^:]+:\d+:[0-9a-f]{32}", run_id):
        raise ValueError(f"{path}: run_id is malformed")
    if kind == "paired":
        if pair_index is None:
            raise AssertionError("paired contract validation requires a pair index")
        order = (
            ["baseline", "candidate"]
            if pair_index % 2 == 1
            else ["candidate", "baseline"]
        )
        require_artifact_value(path, "pair order", contract["pair_order"], order)
        require_artifact_value(
            path, "within-pair ordinal", contract["within_pair_ordinal"], order.index(role) + 1
        )
    else:
        require_artifact_value(path, "pair order", contract["pair_order"], None)
        require_artifact_value(path, "within-pair ordinal", contract["within_pair_ordinal"], None)
    timestamps = {
        name: non_negative_integer(contract[name], name, path)
        for name in (
            "started_unix_ns",
            "started_monotonic_ns",
            "finished_unix_ns",
            "finished_monotonic_ns",
            "duration_ns",
        )
    }
    if any(value == 0 for value in timestamps.values()):
        raise ValueError(f"{path}: run timestamps/duration must be positive")
    if timestamps["finished_unix_ns"] <= timestamps["started_unix_ns"]:
        raise ValueError(f"{path}: finished wall time must follow start")
    if timestamps["finished_monotonic_ns"] <= timestamps["started_monotonic_ns"]:
        raise ValueError(f"{path}: finished monotonic time must follow start")
    require_artifact_value(
        path,
        "duration",
        timestamps["duration_ns"],
        timestamps["finished_monotonic_ns"] - timestamps["started_monotonic_ns"],
    )
    monotonic_clock = contract["monotonic_clock"]
    if not isinstance(monotonic_clock, str) or not monotonic_clock:
        raise ValueError(f"{path}: monotonic clock implementation is missing")
    host = contract["host"]
    if not isinstance(host, dict) or set(host) != set(HOST_IDENTITY_FIELDS):
        raise ValueError(f"{path}: run host identity is malformed")
    if any(not isinstance(host[field], str) or not host[field] for field in host):
        raise ValueError(f"{path}: run host identity contains an empty field")
    require_artifact_value(path, "host identity", host, host_identity)
    return contract


def validate_run_contract_collection(
    evidence_root: Path,
    args: argparse.Namespace,
    scenario: dict[str, Any],
    scenario_hash: str,
    host_identity: dict[str, str],
) -> tuple[dict[Path, dict[str, Any]], list[dict[str, Any]]]:
    contracts: dict[Path, dict[str, Any]] = {}
    chronological: list[dict[str, Any]] = []
    per_geometry = (
        scenario["id"] != "render_and_interaction" and len(scenario["geometry"]) > 1
    )

    def validate_one_run(
        run_path: Path,
        *,
        kind: str,
        role: str,
        pair_index: int | None,
        repeat_index: int | None,
    ) -> list[dict[str, Any]]:
        locations = (
            [
                (run_path / f"geometry-{width}x{height}", geometry_index)
                for geometry_index, (width, height) in enumerate(scenario["geometry"])
            ]
            if per_geometry
            else [(run_path, None)]
        )
        result = []
        for directory, geometry_index in locations:
            contract = validate_run_contract(
                directory / "run-contract.json",
                scenario=scenario,
                scenario_hash=scenario_hash,
                host_identity=host_identity,
                kind=kind,
                role=role,
                pair_index=pair_index,
                repeat_index=repeat_index,
                geometry_index=geometry_index,
            )
            contracts[directory.resolve()] = contract
            result.append(contract)
        return result

    for pair_index, (baseline_path, candidate_path) in enumerate(
        zip(args.baseline_run, args.candidate_run), start=1
    ):
        expected_paths = {
            "baseline": (evidence_root / f"pair-{pair_index:02d}" / "baseline").resolve(),
            "candidate": (evidence_root / f"pair-{pair_index:02d}" / "candidate").resolve(),
        }
        supplied = {"baseline": baseline_path.resolve(), "candidate": candidate_path.resolve()}
        require_artifact_value(
            evidence_root / f"pair-{pair_index:02d}",
            "paired run paths",
            supplied,
            expected_paths,
        )
        pair_contracts: dict[str, list[dict[str, Any]]] = {}
        for role in ("baseline", "candidate"):
            run_path = supplied[role]
            pair_contracts[role] = validate_one_run(
                run_path,
                kind="paired",
                role=role,
                pair_index=pair_index,
                repeat_index=None,
            )
        order = (
            ["baseline", "candidate"]
            if pair_index % 2 == 1
            else ["candidate", "baseline"]
        )
        for role in order:
            chronological.extend(pair_contracts[role])
    for repeat_index, supplied_path in enumerate(args.candidate_repeat_run, start=1):
        expected = (evidence_root / f"candidate-repeat-{repeat_index:02d}").resolve()
        require_artifact_value(expected, "candidate repeat path", supplied_path.resolve(), expected)
        repeat_contracts = validate_one_run(
            expected,
            kind="candidate_repeat",
            role="candidate",
            pair_index=None,
            repeat_index=repeat_index,
        )
        chronological.extend(repeat_contracts)
    run_ids = [contract["run_id"] for contract in chronological]
    if len(run_ids) != len(set(run_ids)):
        raise ValueError("run IDs must be unique across all paired/repeat runs")
    clocks = {contract["monotonic_clock"] for contract in chronological}
    hosts = {json.dumps(contract["host"], sort_keys=True) for contract in chronological}
    if len(clocks) != 1 or len(hosts) != 1:
        raise ValueError("run chronology requires one host and monotonic clock")
    for previous, current in zip(chronological, chronological[1:]):
        if previous["finished_monotonic_ns"] > current["started_monotonic_ns"]:
            raise ValueError(
                f"run chronology overlaps or is reordered: {previous['run_id']} -> {current['run_id']}"
            )
    return contracts, chronological


def validate_run_artifacts(
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_document: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
    seed_context: tuple[dict[str, Any], Path] | None,
    run_contracts: dict[Path, dict[str, Any]],
) -> list[Path]:
    if scenario["id"] == "render_and_interaction":
        run_contract = run_contracts[path.resolve()]
        contract_path = path / "run-contract.json"
        render_path = path / "render.json"
        if not render_path.is_file():
            raise ValueError(f"{path}: missing render.json")
        validate_render_document(
            load_json_object(render_path),
            render_path,
            role,
            scenario,
            scenario_hash,
            host_os,
            manifest,
            run_contract,
        )
        actual = {
            item.resolve()
            for item in path.iterdir()
            if item.is_file() and item.suffix in {".json", ".ndjson"}
        }
        if actual != {render_path.resolve(), contract_path.resolve()}:
            raise ValueError(f"{path}: render run contains unexpected metric JSON/NDJSON")
        return [contract_path, render_path]
    artifacts: list[Path] = []
    directories = process_run_directories(path, scenario)
    if directories != [path]:
        root_metric_files = sorted(
            item.name
            for item in path.iterdir()
            if item.is_file() and item.suffix in {".json", ".ndjson"}
        )
        if root_metric_files:
            raise ValueError(
                f"{path}: multi-geometry root has unexpected metric files {root_metric_files}"
            )
    for directory in directories:
        run_contract = run_contracts[directory.resolve()]
        artifacts.extend(
            validate_process_directory(
                directory,
                role,
                scenario,
                scenario_document,
                scenario_hash,
                host_os,
                manifest,
                seed_context,
                run_contract,
                [directory / "run-contract.json"],
            )
        )
    return artifacts


def measured_mpv_executable_provenance(
    path: Path, scenario: dict[str, Any]
) -> dict[str, Any]:
    identities: set[tuple[str, int, str]] = set()
    for directory in process_run_directories(path, scenario):
        records = read_ndjson(directory / "samples.ndjson")
        for record in records:
            if record.get("kind") != "sample" or record.get("phase") != "measure":
                continue
            for process in record.get("processes", []):
                if isinstance(process, dict) and process.get("role") == "mpv":
                    executable = process.get("executable")
                    executable_bytes = process.get("executable_bytes")
                    executable_sha256 = process.get("executable_sha256")
                    if (
                        not isinstance(executable, str)
                        or not isinstance(executable_bytes, int)
                        or isinstance(executable_bytes, bool)
                        or executable_bytes <= 0
                        or not isinstance(executable_sha256, str)
                    ):
                        raise ValueError(f"{directory}: measured mpv executable identity is malformed")
                    identities.add((executable, executable_bytes, executable_sha256))
    if len(identities) != 1:
        raise ValueError(
            f"{path}: measured geometries/samples used {len(identities)} mpv executable identities"
        )
    executable, executable_bytes, executable_sha256 = next(iter(identities))
    return {
        "executable": executable,
        "executable_bytes": executable_bytes,
        "executable_sha256": executable_sha256,
    }


def relative_artifact(path: Path, root: Path, role: str) -> dict[str, Any]:
    resolved = path.resolve()
    try:
        relative = resolved.relative_to(root).as_posix()
    except ValueError as error:
        raise ValueError(f"artifact must stay inside evidence root {root}: {resolved}") from error
    return {
        "role": role,
        "path": relative,
        "bytes": resolved.stat().st_size,
        "sha256": sha256_file(resolved),
    }


def raw_inventory_digest(entries: list[dict[str, Any]]) -> str:
    digest = hashlib.sha256()
    for entry in sorted(entries, key=lambda item: (item["role"], item["path"])):
        for field in ("role", "path", "sha256"):
            encoded = str(entry[field]).encode("utf-8")
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)
    return digest.hexdigest()


def expand_metric_names(metric: str, runs: list[dict[str, Any]]) -> list[str]:
    if all(metric in run for run in runs):
        return [metric]
    suffix = f".{metric}"
    variants = [
        {name for name in run if name.startswith("geometry.") and name.endswith(suffix)}
        for run in runs
    ]
    if variants and variants[0] and all(names == variants[0] for names in variants[1:]):
        return sorted(variants[0])
    missing = [index for index, names in enumerate(variants) if not names]
    raise ValueError(
        f"required metric {metric!r} is missing or has inconsistent geometry variants "
        f"(empty run indexes: {missing})"
    )


def coefficient_of_variation(values: list[float]) -> float:
    mean = statistics.fmean(values)
    if mean == 0:
        return 0.0 if all(value == 0 for value in values) else math.inf
    return statistics.pstdev(values) / abs(mean)


def linear_slope(points: list[tuple[float, float]]) -> float:
    mean_x = statistics.fmean(point[0] for point in points)
    mean_y = statistics.fmean(point[1] for point in points)
    denominator = sum((x - mean_x) ** 2 for x, _ in points)
    if denominator == 0:
        return 0.0
    return sum((x - mean_x) * (y - mean_y) for x, y in points) / denominator


def paired_bootstrap_ratios(baseline: list[float], candidate: list[float], resamples: int,
                            seed: int, confidence: float) -> tuple[float, float, list[float]]:
    if len(baseline) != len(candidate) or not baseline:
        raise ValueError("paired bootstrap needs equally sized non-empty inputs")
    if any(value < 0 or not math.isfinite(value) for value in baseline + candidate):
        raise ValueError("ratio metrics must be finite and non-negative")
    pair_ratios = [stable_ratio(b, c) for b, c in zip(baseline, candidate)]
    point = stable_ratio(statistics.fmean(baseline), statistics.fmean(candidate))
    rng = random.Random(seed)
    count = len(pair_ratios)
    sampled = []
    for _ in range(resamples):
        indices = [rng.randrange(count) for _ in range(count)]
        sampled.append(stable_ratio(
            statistics.fmean(baseline[index] for index in indices),
            statistics.fmean(candidate[index] for index in indices),
        ))
    sampled.sort()
    upper_index = min(len(sampled) - 1, math.ceil(confidence * len(sampled)) - 1)
    return point, sampled[upper_index], pair_ratios


def stable_ratio(baseline: float, candidate: float) -> float:
    if baseline == 0:
        return 1.0 if candidate == 0 else RATIO_INFINITY
    if candidate == 0:
        return 0.0
    return candidate / baseline


def geometric_mean_ratio(ratios: list[float]) -> float:
    # Conservative ordering for unresolved 0 * infinity products: a regression from an
    # unmeasurably-idle baseline dominates a separate pair that reached exact zero.
    if any(value >= RATIO_INFINITY for value in ratios):
        return RATIO_INFINITY
    if any(value == 0 for value in ratios):
        return 0.0
    return math.exp(statistics.fmean(math.log(value) for value in ratios))


def cleanup_integration_self_test() -> None:
    if os.name == "nt":
        return
    with tempfile.TemporaryDirectory(prefix="ytt-perf-cleanup-self-test-") as temporary:
        root = Path(temporary)
        identity_path = root / "process-identity.json"
        mpv_link = root / "mpv"
        mpv_link.symlink_to(Path(sys.executable).resolve())
        child_script = (
            "import signal,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "time.sleep(60)"
        )
        detached_child_script = (
            "import os,signal,time;os.setsid();"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "time.sleep(60)"
        )
        intermediate_script = (
            "import os,subprocess;"
            "p=subprocess.Popen([os.environ['MPV_LINK'],'-c',os.environ['DETACHED_CHILD_SCRIPT'],"
            "'--input-ipc-server='+os.environ['DETACHED_ENDPOINT']]);"
            "open(os.environ['DETACHED_PID_FILE'],'w').write(str(p.pid));"
        )
        owner_script = (
            "import os,signal,subprocess,sys,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "i=subprocess.Popen([sys.executable,'-c',os.environ['INTERMEDIATE_SCRIPT']]);"
            "m=subprocess.Popen([os.environ['MPV_LINK'],'-c',os.environ['CHILD_SCRIPT'],"
            "'--input-ipc-server='+os.environ['DIRECT_ENDPOINT']]);"
            "o=subprocess.Popen([sys.executable,'-c',os.environ['CHILD_SCRIPT'],'other-child']);"
            "open(os.environ['DIRECT_MPV_PID_FILE'],'w').write(str(m.pid));"
            "open(os.environ['OTHER_PID_FILE'],'w').write(str(o.pid));"
            "i.wait();"
            "time.sleep(60)"
        )
        # This deliberately attempts a stale running write after cleanup_requested. The cleanup
        # command must wait for/kill this TERM-ignoring producer and publish cleaned last.
        producer_script = (
            "import json,os,signal,subprocess,sys,time;os.setsid();"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "p=subprocess.Popen([sys.executable,'-c',os.environ['OWNER_SCRIPT']],"
            "start_new_session=True);"
            "open(os.environ['OWNER_PID_FILE'],'w').write(str(p.pid));"
            "stale=False;"
            "\nwhile True:\n"
            " try:\n"
            "  d=json.load(open(os.environ['IDENTITY_PATH']))\n"
            "  if d.get('state')=='cleanup_requested' and not stale:\n"
            "   d['state']='running';d['cleanup_proven']=False\n"
            "   t=os.environ['IDENTITY_PATH']+'.producer-tmp'\n"
            "   open(t,'w').write(json.dumps(d));os.replace(t,os.environ['IDENTITY_PATH']);stale=True\n"
            " except Exception:\n"
            "  pass\n"
            " time.sleep(0.005)\n"
        )
        environment = os.environ.copy()
        environment.update(
            {
                "MPV_LINK": str(mpv_link),
                "CHILD_SCRIPT": child_script,
                "DETACHED_CHILD_SCRIPT": detached_child_script,
                "INTERMEDIATE_SCRIPT": intermediate_script,
                "OWNER_SCRIPT": owner_script,
                "IDENTITY_PATH": str(identity_path),
                "OWNER_PID_FILE": str(root / "owner.pid"),
                "DETACHED_PID_FILE": str(root / "detached.pid"),
                "DIRECT_MPV_PID_FILE": str(root / "direct-mpv.pid"),
                "OTHER_PID_FILE": str(root / "other.pid"),
                "DETACHED_ENDPOINT": str(root / "detached.sock"),
                "DIRECT_ENDPOINT": str(root / "direct.sock"),
            }
        )
        producer_process = subprocess.Popen(
            [sys.executable, "-c", producer_script],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        controller = subprocess.Popen(
            [sys.executable, "-c", child_script],
            start_new_session=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        expected_pid_files = [
            root / "owner.pid",
            root / "detached.pid",
            root / "direct-mpv.pid",
            root / "other.pid",
        ]
        try:
            ready_deadline = time.monotonic() + 5
            while time.monotonic() < ready_deadline and not all(
                path.is_file() and path.read_text(encoding="utf-8").strip()
                for path in expected_pid_files
            ):
                if producer_process.poll() is not None:
                    raise AssertionError(
                        f"cleanup producer helper exited: {producer_process.stderr.read()}"
                    )
                time.sleep(0.01)
            if not all(path.is_file() for path in expected_pid_files):
                raise AssertionError("cleanup helper topology did not become ready")
            owner_pid, detached_pid, direct_mpv_pid, other_pid = [
                int(path.read_text(encoding="utf-8")) for path in expected_pid_files
            ]
            observations: dict[int, dict[str, Any]] = {}
            observation_deadline = time.monotonic() + 5
            while time.monotonic() < observation_deadline:
                observations = {
                    pid: observation
                    for pid in (
                        producer_process.pid,
                        owner_pid,
                        detached_pid,
                        direct_mpv_pid,
                        other_pid,
                    )
                    if (observation := unix_process_observation(pid, hash_executable=True))
                    is not None
                }
                detached = observations.get(detached_pid)
                if len(observations) == 5 and detached is not None and detached["parent_pid"] != owner_pid:
                    break
                time.sleep(0.02)
            if len(observations) != 5:
                raise AssertionError("cleanup helper processes were not all observable")
            producer_identity = {
                field: observations[producer_process.pid][field]
                for field in (
                    "pid",
                    "start_time_unix_s",
                    "process_group_id",
                    "executable",
                    "executable_bytes",
                    "executable_sha256",
                )
            }
            owner_identity = {
                field: observations[owner_pid][field]
                for field in producer_identity
            }
            # Only the detached/reparented grandchild is pre-recorded. The direct mpv and other
            # child exercise cleanup-time recursive/process-group discovery from an early abort.
            retained = cleanup_descendant_identity(observations[detached_pid])
            if retained["role"] != "mpv":
                raise AssertionError("mpv symlink helper was not classified as mpv")
            initial = {
                "schema": "ytt.tui-perf.live-identity.v1",
                "run_id": "cleanup-integration-self-test",
                "state": "running",
                "producer": producer_identity,
                "owner": owner_identity,
                "partial_owner": None,
                "mpv": mpv_identities_from_descendants([retained]),
                "descendants": [retained],
                "cleanup_scope": CLEANUP_SCOPE,
                "cleanup_proven": False,
                "updated_unix_ns": time.time_ns(),
            }
            atomic_json(identity_path, initial)
            command_cleanup(
                argparse.Namespace(identity=identity_path, timeout_secs=8.0)
            )
            producer_process.wait(timeout=2)
            cleaned = load_json_object(identity_path)
            _producer, _owner, cleaned_mpv, cleaned_descendants = validated_live_identity(
                cleaned, identity_path
            )
            require_artifact_value(identity_path, "cleanup state", cleaned["state"], "cleaned")
            require_artifact_value(identity_path, "cleanup proof", cleaned["cleanup_proven"], True)
            require_artifact_value(
                identity_path, "cleanup scope", cleaned["cleanup_scope"], CLEANUP_SCOPE
            )
            cleaned_pids = {item["pid"] for item in cleaned_descendants}
            if cleaned_pids != {detached_pid, direct_mpv_pid, other_pid}:
                raise AssertionError(
                    f"cleanup descendant inventory mismatch: {cleaned_pids}"
                )
            if {item["pid"] for item in cleaned_mpv} != {detached_pid, direct_mpv_pid}:
                raise AssertionError("cleanup mpv inventory did not preserve/discover both mpv helpers")
            for pid in (producer_process.pid, owner_pid, detached_pid, direct_mpv_pid, other_pid):
                if unix_process_observation(pid, hash_executable=False) is not None:
                    raise AssertionError(f"exact cleanup left helper PID {pid} alive")
            if controller.poll() is not None:
                raise AssertionError("separate controller process group was incorrectly targeted")
            tampered = {**cleaned, "cleanup_scope": "all_host_processes"}
            atomic_json(identity_path, tampered)
            try:
                validated_live_identity(tampered, identity_path)
            except ValueError:
                pass
            else:
                raise AssertionError("tampered cleanup scope must invalidate live identity")
            atomic_json(identity_path, cleaned)
            # Already-cleaned validation must also be idempotent.
            command_cleanup(
                argparse.Namespace(identity=identity_path, timeout_secs=1.0)
            )
        finally:
            for process in (producer_process, controller):
                if process.poll() is None:
                    process.kill()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass


def startup_cleanup_integration_self_test() -> None:
    if os.name == "nt":
        return
    with tempfile.TemporaryDirectory(prefix="ytt-perf-startup-cleanup-self-test-") as temporary:
        root = Path(temporary)
        identity_path = root / "process-identity.json"
        owner_pid_path = root / "owner.pid"
        late_pid_path = root / "late.pid"
        child_script = (
            "import signal,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "time.sleep(60)"
        )
        owner_script = (
            "import os,signal,subprocess,sys,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "p=subprocess.Popen([sys.executable,'-c',os.environ['CHILD_SCRIPT']],"
            "start_new_session=True);"
            "open(os.environ['LATE_PID_PATH'],'w').write(str(p.pid));"
            "time.sleep(60)"
        )
        producer_script = (
            "import os,signal,subprocess,sys,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "p=subprocess.Popen([sys.executable,'-c',os.environ['OWNER_SCRIPT']],"
            "start_new_session=True);"
            "open(os.environ['OWNER_PID_PATH'],'w').write(str(p.pid));"
            "time.sleep(60)"
        )
        environment = os.environ.copy()
        environment.update(
            {
                "CHILD_SCRIPT": child_script,
                "OWNER_SCRIPT": owner_script,
                "OWNER_PID_PATH": str(owner_pid_path),
                "LATE_PID_PATH": str(late_pid_path),
            }
        )
        producer = subprocess.Popen(
            [sys.executable, "-c", producer_script],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        unrelated = subprocess.Popen(
            [sys.executable, "-c", child_script],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not (
                owner_pid_path.is_file() and late_pid_path.is_file()
            ):
                if producer.poll() is not None:
                    raise AssertionError("startup producer exited before publishing its children")
                time.sleep(0.01)
            if not owner_pid_path.is_file() or not late_pid_path.is_file():
                raise AssertionError("startup cleanup topology did not become ready")
            owner_pid = int(owner_pid_path.read_text(encoding="utf-8"))
            late_pid = int(late_pid_path.read_text(encoding="utf-8"))
            producer_observation = unix_process_observation(
                producer.pid, hash_executable=True
            )
            if producer_observation is None:
                raise AssertionError("startup producer was not observable")
            producer_identity = {
                field: producer_observation[field]
                for field in (
                    "pid",
                    "start_time_unix_s",
                    "process_group_id",
                    "executable",
                    "executable_bytes",
                    "executable_sha256",
                )
            }
            atomic_json(
                identity_path,
                {
                    "schema": "ytt.tui-perf.live-identity.v1",
                    "run_id": "startup-cleanup-integration-self-test",
                    "state": "startup",
                    "producer": producer_identity,
                    "owner": None,
                    "partial_owner": None,
                    "mpv": [],
                    "descendants": [],
                    "cleanup_scope": CLEANUP_SCOPE,
                    "cleanup_proven": False,
                    "updated_unix_ns": time.time_ns(),
                },
            )
            command_cleanup(
                argparse.Namespace(identity=identity_path, timeout_secs=8.0)
            )
            producer.wait(timeout=2)
            cleaned = load_json_object(identity_path)
            _cleaned_producer, cleaned_owner, _cleaned_mpv, cleaned_descendants = (
                validated_live_identity(cleaned, identity_path)
            )
            require_artifact_value(identity_path, "startup cleanup state", cleaned["state"], "cleaned")
            require_artifact_value(
                identity_path, "startup cleanup proof", cleaned["cleanup_proven"], True
            )
            require_artifact_value(
                identity_path, "startup cleanup scope", cleaned["cleanup_scope"], CLEANUP_SCOPE
            )
            if cleaned_owner is None or cleaned_owner["pid"] != owner_pid:
                raise AssertionError("startup cleanup did not bind the discovered dedicated owner")
            cleaned_pids = {identity["pid"] for identity in cleaned_descendants}
            if late_pid not in cleaned_pids or owner_pid in cleaned_pids:
                raise AssertionError("startup cleanup did not inventory the late setsid descendant")
            for pid in (producer.pid, owner_pid, late_pid):
                if unix_process_observation(pid, hash_executable=False) is not None:
                    raise AssertionError(f"startup cleanup left exact helper PID {pid} alive")
            if unrelated.poll() is not None:
                raise AssertionError("startup cleanup killed an unrelated same-shell helper")
        finally:
            for process in (producer, unrelated):
                if process.poll() is None:
                    process.kill()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass


def run_contract_integration_self_test() -> None:
    scenario_document, scenario_hash = load_scenarios(DEFAULT_SCENARIOS)
    scenario = find_scenario(scenario_document, "render_and_interaction")
    host_identity = stable_host_identity()
    with tempfile.TemporaryDirectory(prefix="ytt-perf-run-contract-self-test-") as temporary:
        evidence_root = Path(temporary).resolve()
        baseline_runs: list[Path] = []
        candidate_runs: list[Path] = []
        with contextlib.redirect_stdout(io.StringIO()):
            for pair_index in range(1, int(scenario["pairs"]) + 1):
                order = (
                    ("baseline", "candidate")
                    if pair_index % 2 == 1
                    else ("candidate", "baseline")
                )
                for role in order:
                    run_path = evidence_root / f"pair-{pair_index:02d}" / role
                    command_run_start(
                        argparse.Namespace(
                            scenarios=DEFAULT_SCENARIOS,
                            scenario=scenario["id"],
                            output=run_path / "run-contract.json",
                            kind="paired",
                            role=role,
                            pair_index=pair_index,
                            repeat_index=None,
                        )
                    )
                    command_run_finish(
                        argparse.Namespace(contract=run_path / "run-contract.json")
                    )
                baseline_runs.append(evidence_root / f"pair-{pair_index:02d}" / "baseline")
                candidate_runs.append(evidence_root / f"pair-{pair_index:02d}" / "candidate")
        compare_args = argparse.Namespace(
            baseline_run=baseline_runs,
            candidate_run=candidate_runs,
            candidate_repeat_run=[],
        )
        _contracts, chronology = validate_run_contract_collection(
            evidence_root,
            compare_args,
            scenario,
            scenario_hash,
            host_identity,
        )
        expected_roles = [
            role
            for pair_index in range(1, int(scenario["pairs"]) + 1)
            for role in (
                ("baseline", "candidate")
                if pair_index % 2 == 1
                else ("candidate", "baseline")
            )
        ]
        assert [contract["role"] for contract in chronology] == expected_roles

        if len(candidate_runs) > 1:
            swapped_args = argparse.Namespace(
                baseline_run=baseline_runs,
                candidate_run=list(reversed(candidate_runs)),
                candidate_repeat_run=[],
            )
            try:
                validate_run_contract_collection(
                    evidence_root,
                    swapped_args,
                    scenario,
                    scenario_hash,
                    host_identity,
                )
            except ValueError:
                pass
            else:
                raise AssertionError("swapped candidate pair list must be rejected")

        first_path = evidence_root / "pair-01" / "baseline" / "run-contract.json"
        second_path = evidence_root / "pair-01" / "candidate" / "run-contract.json"
        first = load_json_object(first_path)
        second = load_json_object(second_path)
        tampered = dict(second)
        tampered["started_monotonic_ns"] = first["finished_monotonic_ns"] - 1
        tampered["started_unix_ns"] = first["finished_unix_ns"] - 1
        tampered["duration_ns"] = (
            tampered["finished_monotonic_ns"] - tampered["started_monotonic_ns"]
        )
        atomic_json(second_path, tampered)
        try:
            validate_run_contract_collection(
                evidence_root,
                compare_args,
                scenario,
                scenario_hash,
                host_identity,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("overlapping pair chronology must be rejected")
        atomic_json(second_path, second)

        tampered = dict(second)
        tampered["host"] = {
            **second["host"],
            "boot_id_fingerprint": "sha256:" + "0" * 64,
        }
        atomic_json(second_path, tampered)
        try:
            validate_run_contract_collection(
                evidence_root,
                compare_args,
                scenario,
                scenario_hash,
                host_identity,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("cross-boot run contract must be rejected")
        atomic_json(second_path, second)


def multi_geometry_run_contract_integration_self_test() -> None:
    scenario_document, scenario_hash = load_scenarios(DEFAULT_SCENARIOS)
    scenario = find_scenario(scenario_document, "feature_regressions")
    host_identity = stable_host_identity()
    with tempfile.TemporaryDirectory(
        prefix="ytt-perf-multi-geometry-contract-self-test-"
    ) as temporary:
        evidence_root = Path(temporary).resolve()
        baseline_runs: list[Path] = []
        candidate_runs: list[Path] = []
        all_run_paths: list[Path] = []
        with contextlib.redirect_stdout(io.StringIO()):
            for pair_index in range(1, int(scenario["pairs"]) + 1):
                order = (
                    ("baseline", "candidate")
                    if pair_index % 2 == 1
                    else ("candidate", "baseline")
                )
                for role in order:
                    run_path = evidence_root / f"pair-{pair_index:02d}" / role
                    all_run_paths.append(run_path)
                    for geometry_index, (width, height) in enumerate(scenario["geometry"]):
                        geometry_path = run_path / f"geometry-{width}x{height}"
                        command_run_start(
                            argparse.Namespace(
                                scenarios=DEFAULT_SCENARIOS,
                                scenario=scenario["id"],
                                output=geometry_path / "run-contract.json",
                                kind="paired",
                                role=role,
                                pair_index=pair_index,
                                repeat_index=None,
                                geometry_index=geometry_index,
                                width=width,
                                height=height,
                            )
                        )
                        command_run_finish(
                            argparse.Namespace(
                                contract=geometry_path / "run-contract.json"
                            )
                        )
                baseline_runs.append(evidence_root / f"pair-{pair_index:02d}" / "baseline")
                candidate_runs.append(
                    evidence_root / f"pair-{pair_index:02d}" / "candidate"
                )
        if any((run_path / "run-contract.json").exists() for run_path in all_run_paths):
            raise AssertionError("multi-geometry process run must not publish a root contract")
        compare_args = argparse.Namespace(
            baseline_run=baseline_runs,
            candidate_run=candidate_runs,
            candidate_repeat_run=[],
        )
        contracts, chronology = validate_run_contract_collection(
            evidence_root,
            compare_args,
            scenario,
            scenario_hash,
            host_identity,
        )
        expected_order = [
            (role, geometry_index)
            for pair_index in range(1, int(scenario["pairs"]) + 1)
            for role in (
                ("baseline", "candidate")
                if pair_index % 2 == 1
                else ("candidate", "baseline")
            )
            for geometry_index in range(len(scenario["geometry"]))
        ]
        assert [
            (contract["role"], contract["geometry_index"])
            for contract in chronology
        ] == expected_order

        first_width, first_height = scenario["geometry"][0]
        second_width, second_height = scenario["geometry"][1]
        first_directory = (
            evidence_root
            / "pair-01"
            / "baseline"
            / f"geometry-{first_width}x{first_height}"
        )
        second_directory = (
            evidence_root
            / "pair-01"
            / "baseline"
            / f"geometry-{second_width}x{second_height}"
        )
        first_contract = contracts[first_directory.resolve()]
        second_contract = contracts[second_directory.resolve()]
        copied_sampler_record = {"terminal_geometry": scenario["geometry"][0]}
        validate_sampler_terminal_geometry(
            Path("<geometry-self-test>"),
            copied_sampler_record,
            copied_sampler_record,
            first_contract,
        )
        try:
            validate_sampler_terminal_geometry(
                Path("<geometry-copy-self-test>"),
                copied_sampler_record,
                copied_sampler_record,
                second_contract,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("copied sampler geometry must be rejected")

        first_path = first_directory / "run-contract.json"
        second_path = second_directory / "run-contract.json"
        original_second = second_path.read_bytes()
        atomic_bytes(second_path, first_path.read_bytes())
        try:
            validate_run_contract_collection(
                evidence_root,
                compare_args,
                scenario,
                scenario_hash,
                host_identity,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("copied geometry contract must be rejected")
        finally:
            atomic_bytes(second_path, original_second)
        validate_run_contract_collection(
            evidence_root,
            compare_args,
            scenario,
            scenario_hash,
            host_identity,
        )


def toolchain_identity_self_test() -> None:
    shim_source = r'''from pathlib import Path
import json
import os
import sys


def selected_toolchain():
    current = Path.cwd().resolve()
    while True:
        for name in ("rust-toolchain.toml", "rust-toolchain"):
            selector = current / name
            if selector.is_file():
                for line in selector.read_text(encoding="utf-8").splitlines():
                    value = line.strip()
                    if value and not value.startswith("#"):
                        return value
        if current.parent == current:
            return "fake-default"
        current = current.parent


tool = sys.argv[1]
arguments = sys.argv[2:]
selected = selected_toolchain()
if tool == "cargo" and arguments == ["-Vv"]:
    print(f"cargo 1.0.0 ({selected})\\nrelease: 1.0.0\\nhost: fake-host")
elif tool == "cargo" and arguments[:4] == [
    "build", "--release", "--locked", "--message-format=json-render-diagnostics"
]:
    suffix = ".cmd" if sys.platform == "win32" else ""
    expected_rustc = str((Path(sys.argv[0]).resolve().parent / f"rustc{suffix}").resolve())
    if os.environ.get("RUSTC") != expected_rustc:
        print("fake Cargo was not pinned to the captured absolute rustc", file=sys.stderr)
        raise SystemExit(3)
    if os.environ.get("RUSTC_WRAPPER") != "" or os.environ.get("RUSTC_WORKSPACE_WRAPPER") != "":
        print("fake Cargo did not explicitly disable rustc wrappers", file=sys.stderr)
        raise SystemExit(4)
    target = Path(os.environ["CARGO_TARGET_DIR"]) / "release"
    target.mkdir(parents=True, exist_ok=True)
    executable_suffix = ".exe" if sys.platform == "win32" else ""
    executable = target / f"fixture{executable_suffix}"
    executable.write_bytes(f"fixture built by {selected}\\n".encode())
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "fixture"},
        "executable": str(executable),
    }))
elif tool == "rustc" and arguments == ["-vV"]:
    print(f"rustc 1.0.0 ({selected})\\nbinary: rustc\\nhost: fake-host")
elif tool == "rustup" and arguments == ["show", "active-toolchain"]:
    print(f"{selected}-fake-host (fake selector)")
elif tool == "rustup" and arguments == ["override", "list"]:
    print("no overrides")
elif tool == "rustup" and arguments == ["--version"]:
    print("rustup 1.0.0 (fake)")
elif tool == "rustup" and len(arguments) == 2 and arguments[0] == "which":
    suffix = ".cmd" if sys.platform == "win32" else ""
    print(Path(sys.argv[0]).resolve().parent / f"{arguments[1]}{suffix}")
else:
    print(f"unsupported fake tool invocation: {tool} {arguments}", file=sys.stderr)
    raise SystemExit(2)
'''
    with tempfile.TemporaryDirectory(
        prefix="ytt-perf-toolchain-identity-self-test-"
    ) as temporary:
        base = Path(temporary)
        fake_bin = base / "bin"
        fake_bin.mkdir()
        shim = fake_bin / "tool-shim.py"
        shim.write_text(shim_source, encoding="utf-8")
        tool_paths: dict[str, Path] = {}
        for name in ("cargo", "rustc", "rustup"):
            if os.name == "nt":
                path = fake_bin / f"{name}.cmd"
                path.write_text(
                    f'@"{sys.executable}" "{shim}" {name} %*\r\n',
                    encoding="utf-8",
                )
            else:
                path = fake_bin / name
                path.write_text(
                    "#!/bin/sh\n"
                    f"exec {shlex.quote(sys.executable)} {shlex.quote(str(shim))} "
                    f"{name} \"$@\"\n",
                    encoding="utf-8",
                )
                path.chmod(0o755)
            tool_paths[name] = path

        baseline_parent = base / "baseline-parent"
        candidate_parent = base / "candidate-parent"
        baseline_root = baseline_parent / "source"
        candidate_root = candidate_parent / "source"
        baseline_root.mkdir(parents=True)
        candidate_root.mkdir(parents=True)
        (baseline_parent / "rust-toolchain").write_text(
            "fake-ancestor\n", encoding="utf-8"
        )
        (candidate_parent / "rust-toolchain").write_text(
            "fake-ancestor\n", encoding="utf-8"
        )
        hostile_wrapper = base / "hostile-rustc-wrapper"
        hostile_wrapper.write_text("must never execute\n", encoding="utf-8")
        for parent in (baseline_parent, candidate_parent):
            cargo_dir = parent / ".cargo"
            cargo_dir.mkdir()
            (cargo_dir / "config.toml").write_text(
                f'[build]\nrustc-wrapper = "{hostile_wrapper.as_posix()}"\n',
                encoding="utf-8",
            )
        baseline_selector = baseline_root / "rust-toolchain.toml"
        candidate_selector = candidate_root / "rust-toolchain.toml"
        baseline_selector.write_text("fake-a\n", encoding="utf-8")
        candidate_selector.write_text("fake-a\n", encoding="utf-8")
        (candidate_root / ".gitignore").write_text(
            "rust-toolchain.toml\n", encoding="utf-8"
        )
        environment = {
            "PATH": str(fake_bin),
            "HOME": str(base / "home"),
            "CARGO_HOME": str(base / "cargo-home"),
            "RUSTUP_HOME": str(base / "rustup-home"),
            "PATHEXT": ".COM;.EXE;.BAT;.CMD",
        }
        recorded = capture_build_toolchains(
            baseline_root, candidate_root, environment
        )
        selector_scopes = [
            (entry["scope"], entry["name"])
            for entry in recorded["candidate"]["selector_chain"]
        ]
        if selector_scopes != [
            ("source", "rust-toolchain.toml"),
            ("ancestor", "rust-toolchain"),
        ]:
            raise AssertionError(
                f"ignored/ancestor toolchain selector chain was not captured: {selector_scopes}"
            )
        parsed_overrides = relevant_rustup_overrides(
            candidate_root,
            f"{candidate_parent.resolve()}\tfake-a\n"
            f"{(base / 'unrelated').resolve()}\tfake-z",
        )
        if len(parsed_overrides) != 1 or parsed_overrides[0]["ancestor_depth"] != 1:
            raise AssertionError("relevant rustup directory override was not isolated")

        candidate_selector.write_text("fake-b\n", encoding="utf-8")
        try:
            capture_build_toolchains(baseline_root, candidate_root, environment)
        except ValueError:
            pass
        else:
            raise AssertionError("different effective per-root toolchains must fail before build")

        candidate_selector.write_text("fake-a\n# ignored semantic comment\n", encoding="utf-8")
        try:
            validate_recorded_build_toolchains(
                recorded, baseline_root, candidate_root, environment
            )
        except ValueError:
            pass
        else:
            raise AssertionError("tampered ignored selector identity must invalidate the receipt")
        candidate_selector.write_bytes(baseline_selector.read_bytes())
        validate_recorded_build_toolchains(
            recorded, baseline_root, candidate_root, environment
        )

        cargo_path = tool_paths["cargo"]
        original_cargo = cargo_path.read_bytes()
        cargo_path.write_bytes(
            original_cargo
            + (b"\r\nrem identity tamper\r\n" if os.name == "nt" else b"\n# identity tamper\n")
        )
        try:
            validate_recorded_build_toolchains(
                recorded, baseline_root, candidate_root, environment
            )
        except ValueError:
            pass
        else:
            raise AssertionError("tampered Cargo executable identity must invalidate the receipt")
        cargo_path.write_bytes(original_cargo)
        validate_recorded_build_toolchains(
            recorded, baseline_root, candidate_root, environment
        )
        command, executables = run_fixed_cargo_build(
            candidate_root,
            base / "target",
            ["--bin", "fixture"],
            environment,
            recorded["candidate"],
        )
        if command[-2:] != ["--bin", "fixture"] or set(executables) != {"fixture"}:
            raise AssertionError("controlled fake Cargo build did not emit the targeted fixture")
        binding = pinned_compiler_binding(recorded["candidate"])
        if binding["environment"] != {
            "RUSTC": str(tool_paths["rustc"].resolve()),
            "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
        }:
            raise AssertionError("controlled build compiler binding is not exact")


def host_identity_privacy_self_test() -> None:
    system = platform.system()
    raw_identifiers = (
        platform.node(),
        stable_machine_id(system),
        stable_boot_id(system),
    )
    identity = stable_host_identity()
    serialized = json.dumps(identity, sort_keys=True)
    for field in (
        "node_fingerprint",
        "machine_id_fingerprint",
        "boot_id_fingerprint",
    ):
        value = identity[field]
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", value):
            raise AssertionError(f"host identity {field} is not a SHA-256 fingerprint")
    for raw in raw_identifiers:
        if raw in serialized:
            raise AssertionError("raw host identifier leaked into the serialized identity")
        if ":" in raw and raw.split(":", 1)[1] in serialized:
            raise AssertionError("raw host identifier payload leaked into the serialized identity")


def command_self_test(_args: argparse.Namespace) -> int:
    scenario_validation_self_test()
    tree_digest_self_test()
    effective_worktree_digest_self_test()
    mpv_cache_argv_contract_self_test()
    toolchain_identity_self_test()
    cleanup_integration_self_test()
    startup_cleanup_integration_self_test()
    host_identity_privacy_self_test()
    run_contract_integration_self_test()
    multi_geometry_run_contract_integration_self_test()
    launch_policy_self_test()
    child_environment_policy_self_test()
    materialized_session_self_test()
    materialize_command_self_test()
    seed_path_containment_self_test()
    http_pacing_self_test()
    http_server_shutdown_self_test()
    control_operations_self_test()
    sample_tree_topology_self_test()
    point, upper, ratios = paired_bootstrap_ratios([0.0, 0.0], [0.0, 0.0], 100, 7, 0.95)
    assert point == 1.0 and upper == 1.0 and ratios == [1.0, 1.0]
    point, upper, ratios = paired_bootstrap_ratios([1.0, 2.0], [0.0, 0.0], 100, 7, 0.95)
    assert point == 0.0 and upper == 0.0 and ratios == [0.0, 0.0]
    point, upper, ratios = paired_bootstrap_ratios([0.0, 1.0], [0.1, 1.0], 100, 7, 0.95)
    assert point == 1.1 and upper == RATIO_INFINITY
    assert ratios == [RATIO_INFINITY, 1.0]
    point, upper, ratios = paired_bootstrap_ratios(
        [1.0] * 7, [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 100.0], 10_000, 7, 0.95
    )
    assert point > 10.0 and upper > 10.0
    assert ratios == [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 100.0]
    document = {"geometry": [[80, 24], [160, 50]]}
    assert dotted(document, "geometry.length") == 2
    assert dotted(document, "geometry.1.0") == 160
    geometry_runs = [
        {
            "geometry.100x30.tree.mean_cpu_percent": 1.0,
            "geometry.160x50.tree.mean_cpu_percent": 2.0,
        },
        {
            "geometry.100x30.tree.mean_cpu_percent": 0.5,
            "geometry.160x50.tree.mean_cpu_percent": 1.0,
        },
    ]
    assert expand_metric_names("tree.mean_cpu_percent", geometry_runs) == [
        "geometry.100x30.tree.mean_cpu_percent",
        "geometry.160x50.tree.mean_cpu_percent",
    ]
    try:
        json.loads('{"metric":1,"metric":2}', object_pairs_hook=reject_duplicate_json_keys)
    except DuplicateJsonKeyError:
        pass
    else:
        raise AssertionError("duplicate scenario keys must be rejected")
    render_batch = {
        "draws": 200,
        "total_ns": 2_000,
        "mean_draw_ns": 10,
        "p50_draw_ns": 10,
        "p95_draw_ns": 10,
        "max_draw_ns": 10,
        "latency_histogram": [{"ns": 10, "count": 200}],
        "allocations": 200,
        "reallocations": 0,
        "allocated_bytes": 400,
        "deallocated_bytes": 400,
        "retained_bytes_delta": 0,
        "peak_live_bytes_delta": 2,
    }
    render_document = {
        "schema": "ytt.tui-perf.render.v1",
        "cases": [{
            "name": "pooled",
            "update_path": "app_update_msg_key",
            "measured_draws": 400,
            "total_draw_ns": 4_000,
            "mean_draw_ns": 10,
            "p50_draw_ns": 10,
            "p95_draw_ns": 10,
            "max_draw_ns": 10,
            "latency_histogram": [{"ns": 10, "count": 400}],
            "batches": [render_batch, render_batch],
            "buffer_style_digest": "buffer",
            "hit_map_digest": "hits",
        }],
    }
    render_metrics = render_metrics_from_document(render_document, Path("<self-test>"))
    assert render_metrics["render.pooled.p95_draw_ns"] == 10
    assert render_metrics["render.pooled.p95_reducer_input_to_draw_ns"] == 10
    render_document["cases"][0]["p95_draw_ns"] = 11
    try:
        render_metrics_from_document(render_document, Path("<self-test>"))
    except ValueError:
        pass
    else:
        raise AssertionError("render p95 tampering must be rejected from raw histogram")
    render_document["cases"][0]["p95_draw_ns"] = 10
    render_document["cases"][0]["measured_draws"] = 399
    try:
        render_metrics_from_document(render_document, Path("<self-test>"))
    except ValueError:
        pass
    else:
        raise AssertionError("render measured_draws mismatch must be rejected")
    render_document["cases"][0]["measured_draws"] = 400
    render_document["cases"][0]["batches"][0]["retained_bytes_delta"] = 1
    try:
        render_metrics_from_document(render_document, Path("<self-test>"))
    except ValueError:
        pass
    else:
        raise AssertionError("render allocator byte-conservation tampering must be rejected")
    render_document["cases"][0]["batches"][0]["retained_bytes_delta"] = 0
    identity_scenario = {
        "id": "render_and_interaction",
        "warmup_draws": 2,
        "batches": 1,
        "draws_per_batch": 3,
    }
    identity_manifest = {
        "binaries": {
            "baseline_render": {"sha256": "ab" * 32},
            "candidate_render": {"sha256": "cd" * 32},
        }
    }
    identity_document = {
        "schema": "ytt.tui-perf.render.v1",
        "kind": "render_summary",
        "scenario_sha256": "scenario",
        "binary_sha256": "ab" * 32,
        "run_id": "render:self-test",
        "started_unix_ns": 200,
        "finished_unix_ns": 300,
        "os": normalized_os(platform.system()),
        "batches_per_case": 1,
        "draws_per_batch": 3,
        "cases": [
            {
                "name": "identity",
                "update_path": "app_update_msg_key",
                "warmup_draws": 2,
                "measured_draws": 3,
                "total_draw_ns": 15,
                "mean_draw_ns": 5,
                "p50_draw_ns": 5,
                "p95_draw_ns": 5,
                "max_draw_ns": 5,
                "latency_histogram": [{"ns": 5, "count": 3}],
                "buffer_style_digest": "buffer",
                "hit_map_digest": "hits",
                "batches": [
                    {
                        "draws": 3,
                        "total_ns": 15,
                        "mean_draw_ns": 5,
                        "p50_draw_ns": 5,
                        "p95_draw_ns": 5,
                        "max_draw_ns": 5,
                        "latency_histogram": [{"ns": 5, "count": 3}],
                        "allocations": 3,
                        "reallocations": 0,
                        "allocated_bytes": 30,
                        "deallocated_bytes": 30,
                        "retained_bytes_delta": 0,
                        "peak_live_bytes_delta": 10,
                    }
                ],
            }
        ],
    }
    identity_run_contract = {
        "run_id": "render:self-test",
        "started_unix_ns": 100,
        "finished_unix_ns": 400,
    }
    validate_render_document(
        identity_document,
        Path("<identity-self-test>"),
        "baseline",
        identity_scenario,
        "scenario",
        normalized_os(platform.system()),
        identity_manifest,
        identity_run_contract,
    )
    identity_document["binary_sha256"] = "00" * 32
    try:
        validate_render_document(
            identity_document,
            Path("<identity-self-test>"),
            "baseline",
            identity_scenario,
            "scenario",
            normalized_os(platform.system()),
            identity_manifest,
            identity_run_contract,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("render binary hash tampering must be rejected")
    identity_document["binary_sha256"] = "ab" * 32
    identity_document["scenario_sha256"] = "tampered"
    try:
        validate_render_document(
            identity_document,
            Path("<identity-self-test>"),
            "baseline",
            identity_scenario,
            "scenario",
            normalized_os(platform.system()),
            identity_manifest,
            identity_run_contract,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("render scenario hash tampering must be rejected")
    identity_document["scenario_sha256"] = "scenario"
    identity_document["run_id"] = "swapped-run"
    try:
        validate_render_document(
            identity_document,
            Path("<identity-self-test>"),
            "baseline",
            identity_scenario,
            "scenario",
            normalized_os(platform.system()),
            identity_manifest,
            identity_run_contract,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("render run ID tampering must be rejected")
    identity_document["run_id"] = identity_run_contract["run_id"]
    with tempfile.TemporaryDirectory(prefix="ytt-perf-self-test-") as temporary:
        root = Path(temporary)
        raw = root / "raw.json"
        sums = root / "SHA256SUMS"
        raw.write_text('{"value":1}\n', encoding="utf-8")
        assert write_checksums(root, sums) == 1
        assert verify_checksums(root, sums) == 1
        shadow_dir = root / "nested"
        shadow_dir.mkdir()
        (shadow_dir / "SHA256SUMS.tmp").write_text("must be inventoried\n", encoding="utf-8")
        assert write_checksums(root, sums) == 2
        assert verify_checksums(root, sums) == 2
        raw.write_text('{"value":2}\n', encoding="utf-8")
        sums_before = sha256_file(sums)
        try:
            verify_checksums(root, sums)
        except ValueError:
            pass
        else:
            raise AssertionError("raw artifact tampering must be rejected")
        assert sha256_file(sums) == sums_before

        base = root / "seed"
        overlay = root / "overlay"
        (base / "stores").mkdir(parents=True)
        overlay.mkdir()
        (base / "stores" / "state.json").write_text('{"old":true}\n', encoding="utf-8")
        (overlay / "stores").mkdir()
        (overlay / "stores" / "state.json").write_text('{"old":false}\n', encoding="utf-8")
        overlay_digest, overlay_files = overlay_tree_identity(
            base, overlay, ["stores/state.json"]
        )
        assert overlay_digest != sha256_tree(base)
        assert overlay_files[0]["sha256"] == sha256_file(overlay / "stores" / "state.json")
        try:
            overlay_tree_identity(base, overlay, ["wrong.json"])
        except ValueError:
            pass
        else:
            raise AssertionError("overlay path tampering must be rejected")

        evidence = root / "evidence"
        snapshot = evidence / "seed-template"
        for store in ("config", "data", "cache"):
            (snapshot / "stores" / store).mkdir(parents=True, exist_ok=True)
        (snapshot / "stores" / "config" / "config.json").write_text(
            '{"audio":{"mpv":{}}}\n', encoding="utf-8"
        )
        atomic_json(
            snapshot / "stores" / "cache" / "session.json",
            {
                "schema_version": 2,
                "app_version": project_package_version(),
                "last_mode": "normal",
                "normal_queue": {
                    "songs": [{"local_path": "{{TUI_PERF_PLAYLIST}}"}],
                    "order": [0],
                    "cursor": 0,
                },
            },
        )
        seed_digest = sha256_tree(snapshot)
        seed_scenario = {
            "id": "seed-self-test",
            "seed_contract": {
                "require_identical_tree": True,
                "require_identical_cache_policy": True,
                "expected_cache_policy": None,
            },
        }
        seed_manifest_path = evidence / "seed-contract.json"
        seed_manifest = {
            "schema": SEED_CONTRACT_SCHEMA,
            "scenario": seed_scenario["id"],
            "scenario_sha256": "seed-scenario",
            "contract": seed_scenario["seed_contract"],
            "source_tree_sha256": {
                "baseline": seed_digest,
                "candidate": seed_digest,
            },
            "cache_policy": seed_cache_policy(snapshot),
            "playlist_placeholder_count": {"baseline": 1, "candidate": 1},
            "active_playlist_contract": validate_active_session_playlist(
                snapshot, "{{TUI_PERF_PLAYLIST}}"
            ),
            "snapshot": "seed-template",
            "snapshot_tree_sha256": seed_digest,
            "snapshot_files": tree_file_inventory(snapshot),
        }
        atomic_json(seed_manifest_path, seed_manifest)
        validate_seed_contract_manifest(
            seed_manifest_path, evidence, seed_scenario, "seed-scenario"
        )
        seed_manifest["cache_policy"] = {"config_present": False}
        atomic_json(seed_manifest_path, seed_manifest)
        try:
            validate_seed_contract_manifest(
                seed_manifest_path, evidence, seed_scenario, "seed-scenario"
            )
        except ValueError:
            pass
        else:
            raise AssertionError("seed cache-policy manifest tampering must be rejected")

        def git_checked(cwd: Path, *arguments: str) -> None:
            completed = subprocess.run(
                ["git", "-C", str(cwd), *arguments],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if completed.returncode != 0:
                raise AssertionError(
                    f"self-test git {' '.join(arguments)} failed: {completed.stderr.strip()}"
                )

        candidate_source = root / "candidate-source"
        baseline_source = root / "baseline-source"
        remote_source = root / "origin.git"
        candidate_source.mkdir()
        git_checked(candidate_source, "init", "--initial-branch=main")
        git_checked(candidate_source, "config", "user.name", "tui-perf self-test")
        git_checked(candidate_source, "config", "user.email", "tui-perf@example.invalid")
        (candidate_source / "Cargo.lock").write_text("lock\n", encoding="utf-8")
        (candidate_source / "Cargo.toml").write_text(
            '[package]\nname = "tui-perf-self-test"\nversion = "0.0.1"\n',
            encoding="utf-8",
        )
        (candidate_source / ".gitignore").write_text("/.cargo/\n", encoding="utf-8")
        git_checked(candidate_source, "add", "Cargo.lock", "Cargo.toml", ".gitignore")
        git_checked(candidate_source, "-c", "commit.gpgsign=false", "commit", "-m", "main")
        remote_source.mkdir()
        git_checked(remote_source, "init", "--bare", "--initial-branch=main")
        git_checked(candidate_source, "remote", "add", "origin", str(remote_source))
        git_checked(candidate_source, "push", "-u", "origin", "main")
        git_checked(candidate_source, "switch", "-c", "candidate")
        render_harness = candidate_source / "examples" / "tui_render_perf.rs"
        render_harness.parent.mkdir()
        render_harness.write_text("fn main() {}\n", encoding="utf-8")
        git_checked(candidate_source, "add", "examples/tui_render_perf.rs")
        git_checked(
            candidate_source,
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "candidate",
        )
        completed = subprocess.run(
            ["git", "clone", str(remote_source), str(baseline_source)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            raise AssertionError(f"self-test git clone failed: {completed.stderr.strip()}")
        validate_source_contract(
            baseline_source, candidate_source, render=True, refresh=True
        )

        (candidate_source / "Cargo.lock").write_text("dirty\n", encoding="utf-8")
        try:
            validate_source_contract(
                baseline_source, candidate_source, render=True, refresh=False
            )
        except ValueError:
            pass
        else:
            raise AssertionError("dirty candidate source must be rejected")
        (candidate_source / "Cargo.lock").write_text("lock\n", encoding="utf-8")

        baseline_harness = baseline_source / "examples" / "tui_render_perf.rs"
        baseline_harness.write_text("fn main() { panic!(); }\n", encoding="utf-8")
        try:
            validate_source_contract(
                baseline_source, candidate_source, render=True, refresh=False
            )
        except ValueError:
            pass
        else:
            raise AssertionError("mismatched baseline render harness must be rejected")
        baseline_harness.write_bytes(render_harness.read_bytes())

        ignored_config = candidate_source / ".cargo" / "config.toml"
        ignored_config.parent.mkdir()
        ignored_config.write_text('[build]\nrustflags = ["-Ctarget-cpu=native"]\n', encoding="utf-8")
        try:
            validate_source_contract(
                baseline_source, candidate_source, render=True, refresh=False
            )
        except ValueError:
            pass
        else:
            raise AssertionError("ignored candidate Cargo config difference must be rejected")
        ignored_config.unlink()
        ignored_config.parent.rmdir()

        orphan_oid = str(
            run_git(
                candidate_source,
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                "HEAD^{tree}",
                "-m",
                "disconnected candidate",
            )
        )
        git_checked(candidate_source, "switch", "--detach", orphan_oid)
        try:
            validate_source_contract(
                baseline_source, candidate_source, render=True, refresh=False
            )
        except ValueError:
            pass
        else:
            raise AssertionError("candidate not descended from origin/main must be rejected")
        git_checked(candidate_source, "switch", "candidate")

        (baseline_source / "unexpected.txt").write_text("unexpected\n", encoding="utf-8")
        try:
            validate_source_contract(
                baseline_source, candidate_source, render=True, refresh=False
            )
        except ValueError:
            pass
        else:
            raise AssertionError("unexpected baseline untracked files must be rejected")

    measured_header = {
        "root_pid": 100,
        "binary_sha256": "ab" * 32,
        "warmup_ms": 1_000,
        "duration_ms": 2_000,
        "cpu_accounting": CPU_ACCOUNTING_METHOD,
        "cpu_window_start_ns": 1_000_000_000,
        "cpu_window_end_ns": 3_000_000_000,
        "interval_ms": 1_000,
    }

    def raw_sample(
        elapsed_ms: int,
        cpu_overlap_ms: int,
        phase: str,
        accumulated_ms: int,
        cpu: float,
        rss: int,
    ) -> dict[str, Any]:
        role = {"processes": 1, "cpu_percent": cpu, "rss_bytes": rss}
        return {
            "schema": "ytt.tui-perf.samples.v1",
            "kind": "sample",
            "elapsed_ms": elapsed_ms,
            "observed_elapsed_ns": elapsed_ms * 1_000_000,
            "cpu_interval_overlap_ns": cpu_overlap_ms * 1_000_000,
            "phase": phase,
            "mpv_present": False,
            "mpv_all_silent_this_sample": False,
            "roles": {"ytt": dict(role), "tree": dict(role)},
            "processes": [
                {
                    "pid": 100,
                    "parent_pid": 1,
                    "role": "ytt",
                    "name": "ytt",
                    "start_time_unix_s": 50,
                    "accumulated_cpu_ms": accumulated_ms,
                    "cpu_percent": cpu,
                    "rss_bytes": rss,
                    "command": ["/tmp/ytt"],
                    "executable": "/tmp/ytt",
                    "executable_bytes": 1,
                    "executable_sha256": "ab" * 32,
                }
            ],
        }

    measured_records = [
        raw_sample(0, 0, "warmup", 0, 0.0, 50),
        raw_sample(1_100, 100, "measure", 110, 10.0, 100),
        raw_sample(1_900, 800, "measure", 270, 20.0, 200),
        raw_sample(3_100, 1_100, "measure", 630, 30.0, 300),
    ]
    measured_summary = {
        "cpu_accounting": CPU_ACCOUNTING_METHOD,
        "cpu_window_start_ns": 1_000_000_000,
        "cpu_window_end_ns": 3_000_000_000,
        "roles": {
            "ytt": {
                "samples": 3,
                "mean_cpu_percent": 25.0,
                "mean_rss_bytes": 200,
                "peak_rss_bytes": 300,
            },
            "tree": {
                "samples": 3,
                "mean_cpu_percent": 25.0,
                "mean_rss_bytes": 200,
                "peak_rss_bytes": 300,
            }
        },
        "root_pid": 100,
        "silent_mpv_proven": False,
        "measured_mpv_proof": {
            "samples": 3,
            "samples_with_mpv": 0,
            "samples_all_silent": 0,
            "samples_all_cleanup_identified": 0,
        },
        "last_observed_mpv": [],
    }
    validate_measured_samples(
        Path("<sample-self-test>"), measured_header, measured_summary, measured_records, False
    )
    measured_summary["roles"]["tree"]["mean_cpu_percent"] = 1.0
    try:
        validate_measured_samples(
            Path("<sample-self-test>"), measured_header, measured_summary, measured_records, False
        )
    except ValueError:
        pass
    else:
        raise AssertionError("sampler summary tampering must be rejected")
    measured_summary["roles"]["tree"]["mean_cpu_percent"] = 25.0
    measured_records[2]["elapsed_ms"] = 1_001
    try:
        validate_measured_samples(
            Path("<sample-self-test>"), measured_header, measured_summary, measured_records, False
        )
    except ValueError:
        pass
    else:
        raise AssertionError("sampler coverage tampering must be rejected")
    measured_records[2]["elapsed_ms"] = 1_900

    overlap_tamper = json.loads(json.dumps(measured_records))
    overlap_tamper[-1]["cpu_interval_overlap_ns"] -= 1
    try:
        validate_measured_samples(
            Path("<sample-self-test>"), measured_header, measured_summary, overlap_tamper, False
        )
    except ValueError:
        pass
    else:
        raise AssertionError("sampler CPU overlap tampering must be rejected")

    try:
        validate_measured_samples(
            Path("<sample-self-test>"),
            measured_header,
            measured_summary,
            measured_records[:-1],
            False,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("sampler omitted final CPU interval must be rejected")

    control_records = [
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": 2_000_000,
            "event": {
                "event": "property-change",
                "name": "paused-for-cache",
                "data": True,
            },
        },
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": 7_000_000,
            "event": {
                "event": "property-change",
                "name": "paused-for-cache",
                "data": False,
            },
        },
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": 9_000_000,
            "event": {
                "event": "property-change",
                "name": "paused-for-cache",
                "data": True,
            },
        },
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": 11_000_000,
            "event": {
                "event": "property-change",
                "name": "paused-for-cache",
                "data": False,
            },
        },
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": 12_000_000,
            "event": {
                "event": "property-change",
                "name": "paused-for-cache",
                "data": True,
            },
        },
    ]
    control_summary = {
        "schema": "ytt.tui-perf.control.v1",
        "kind": "summary",
        "elapsed_ns": 14_000_000,
        "buffering_cutoff_ns": 10_000_000,
        "buffering_events": 2,
        "buffering_ms": 6,
    }
    validate_control_buffering(
        Path("<control-self-test>"), control_records, control_summary
    )
    control_summary["buffering_ms"] = 7
    try:
        validate_control_buffering(
            Path("<control-self-test>"), control_records, control_summary
        )
    except ValueError:
        pass
    else:
        raise AssertionError("controller buffering summary tampering must be rejected")

    progress_records = [
        {
            "schema": "ytt.tui-perf.control.v1",
            "kind": "mpv_event",
            "elapsed_ns": elapsed_ns,
            "event": {
                "event": "property-change",
                "name": "time-pos",
                "data": position_s,
            },
        }
        for elapsed_ns, position_s in (
            (1_000_000_000, 5.0),
            (79_000_000_000, 75.0),
            (81_000_000_000, 999.0),
        )
    ]
    progress_summary = {
        "buffering_cutoff_ns": 80_000_000_000,
        "cutoff_first_time_pos_ns": 1_000_000_000,
        "cutoff_first_time_pos_s": 5.0,
        "cutoff_last_time_pos_ns": 79_000_000_000,
        "cutoff_last_time_pos_s": 75.0,
    }
    steady_scenario = {
        "minimum_playback_progress_fraction": 0.8,
        "time_pos_tail_tolerance_s": 2.0,
    }
    validate_steady_playback_progress(
        Path("<control-progress-self-test>"),
        progress_records,
        progress_summary,
        steady_scenario,
    )
    stalled_records = json.loads(json.dumps(progress_records))
    stalled_records[1]["event"]["data"] = 5.01
    stalled_summary = {
        **progress_summary,
        "cutoff_last_time_pos_s": 5.01,
    }
    try:
        validate_steady_playback_progress(
            Path("<control-progress-self-test>"),
            stalled_records,
            stalled_summary,
            steady_scenario,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("stalled steady playback must be rejected")

    inventory_entry = {"role": "baseline", "path": "run/samples.ndjson", "sha256": "ab" * 32}
    candidate_entry = {**inventory_entry, "role": "candidate"}
    assert raw_inventory_digest([inventory_entry]) != raw_inventory_digest([candidate_entry])
    scenario_document, _ = load_scenarios(DEFAULT_SCENARIOS)
    for replacement in (None, {}):
        invalid = json.loads(json.dumps(scenario_document))
        if replacement is None:
            invalid["scenarios"][0].pop("metrics")
        else:
            invalid["scenarios"][0]["metrics"] = replacement
        try:
            validate_scenarios(invalid)
        except ValueError:
            pass
        else:
            raise AssertionError("missing or empty scenario metrics must be rejected")
    playback_scenarios = [
        scenario for scenario in scenario_document["scenarios"] if scenario["requires_mpv"]
    ]
    assert playback_scenarios and all(
        scenario["expected_effective_mpv_cache_args"]
        == REQUIRED_PLAYBACK_MPV_CACHE_ARGS
        for scenario in playback_scenarios
    )
    missing_cache_contract = json.loads(json.dumps(scenario_document))
    next(
        scenario
        for scenario in missing_cache_contract["scenarios"]
        if scenario["requires_mpv"]
    ).pop("expected_effective_mpv_cache_args")
    try:
        validate_scenarios(missing_cache_contract)
    except ValueError:
        pass
    else:
        raise AssertionError("missing playback mpv cache argv contract must be rejected")
    soak = find_scenario(scenario_document, "memory_soak")
    assert soak["pause_policy"] == "none" and soak["pause_hold_ms"] == 0
    unix_wrapper = Path(__file__).with_name("tui-perf.sh").read_text(encoding="utf-8")
    windows_wrapper = Path(__file__).with_name("tui-perf.ps1").read_text(encoding="utf-8")
    for wrapper in (unix_wrapper, windows_wrapper):
        for token in (
            "--build-receipt",
            "create-checksums",
            "verify-checksums",
            "baseline_render",
            "candidate_render",
            "run-start",
            "run-finish",
            "TUI_PERF_RUN_ID",
        ):
            assert token in wrapper
        for forbidden in (
            "--baseline-binary",
            "--candidate-binary",
            "--baseline-build-command",
            "--candidate-build-command",
        ):
            assert forbidden not in wrapper
    for token in (
        "--geometry-index",
        "--width",
        "--height",
        '"$geometry_dir/run-contract.json"',
        "if $is_render || ((geometry_count == 1)); then",
    ):
        assert token in unix_wrapper
    for token in (
        "pause_policy",
        "pause_hold_ms",
        "--pause-hold-ms",
        "--no-pause",
        "seed-contract",
        "path-preflight",
        "--input-snapshot",
        "sampling.interval_ms",
        "baseline_ytt",
        "candidate_ytt",
        "launch-policy",
        "--identity-file",
        " cleanup ",
        "stop-server",
        "--shutdown-token",
    ):
        assert token in unix_wrapper
    assert unix_wrapper.index("path-preflight") < unix_wrapper.index('mkdir -p "$output"')
    assert unix_wrapper.index("path-preflight") < unix_wrapper.index(
        'python3 "$python_tool" validate'
    )
    assert '"--shutdown-token=$shutdown_token"' in unix_wrapper
    assert '--shutdown-token "$shutdown_token"' not in unix_wrapper
    assert 'kill "$active_server_pid"' not in unix_wrapper
    for token in (
        "off-screen ConPTY",
        "controlled empty input",
        "No process measurement was started and no output was created",
        "if (-not $isRender) { Assert-WindowsProcessIsolation }",
        "path-preflight",
        "--output-root $Output",
        "--protected-root $BaselineSourceRoot",
        "--protected-root $CandidateSourceRoot",
    ):
        assert token in windows_wrapper
    assert windows_wrapper.index("if (-not $isRender)") < windows_wrapper.index("$script:OutputRoot")
    windows_preflight = windows_wrapper.index("$resolvedOutput = & python $PythonTool path-preflight")
    windows_output_create = windows_wrapper.index(
        "New-Item -ItemType Directory -Path $script:OutputRoot"
    )
    assert windows_preflight < windows_output_create
    assert windows_preflight < windows_wrapper.index("$buildArgs = @(")
    for forbidden in (
        "Run-Process",
        "Stop-RecordedYtt",
        "RawUI",
        "baseline_ytt",
        "candidate_ytt",
        'SetEnvironmentVariable("YTM_PERF", "1"',
        '$env:YTM_PERF = "1"',
    ):
        assert forbidden not in windows_wrapper
    parser = build_parser()
    subcommands = next(
        action
        for action in parser._actions
        if isinstance(action, argparse._SubParsersAction)
    ).choices
    assert "create-checksums" in subcommands
    assert "verify-checksums" in subcommands
    assert "checksums" not in subcommands
    print(
        json.dumps(
            {
                "ok": True,
                "zero_ratio_cases": 4,
                "geometry_variant_cases": 2,
                "multi_geometry_contract_cases": 2,
                "duplicate_key_cases": 1,
                "aggregate_render_p95_cases": 2,
                "render_identity_tamper_cases": 2,
                "checksum_tamper_cases": 1,
                "checksum_shadow_inventory_cases": 1,
                "tree_digest_collision_cases": 1,
                "tree_nonregular_rejection_cases": 1,
                "effective_worktree_digest_collision_cases": 1,
                "effective_worktree_digest_fail_closed_cases": 1,
                "overlay_tamper_cases": 1,
                "seed_manifest_tamper_cases": 1,
                "seed_path_overlap_subprocess_cases": 2,
                "sample_summary_tamper_cases": 1,
                "sample_coverage_tamper_cases": 1,
                "sample_cpu_window_tamper_cases": 2,
                "sample_jitter_weighting_cases": 1,
                "control_buffering_tamper_cases": 1,
                "scenario_schema_tamper_cases": 33,
                "control_operation_tamper_cases": 13,
                "http_server_authenticated_shutdown_cases": 1,
                "http_server_leading_dash_token_cases": 1,
                "http_server_stale_pid_no_signal_cases": 1,
                "sample_tree_topology_tamper_cases": 4,
                "source_contract_tamper_cases": 3,
                "toolchain_identity_tamper_cases": 3,
                "cleanup_scope_tamper_cases": 1,
                "raw_role_binding_cases": 1,
                "steady_soak_pause_cases": 1,
                "wrapper_pause_parity_cases": 2,
                "wrapper_contract_parity_cases": 2,
                "wrapper_shutdown_token_argv_cases": 2,
                "checksum_command_split_cases": 1,
            }
        )
    )
    return 0


def compare_metric(name: str, policy: dict[str, Any], baseline: list[Any], candidate: list[Any],
                   scenario: dict[str, Any], stats: dict[str, Any], seed_offset: int) -> dict[str, Any]:
    comparison = policy.get("comparison", "ratio")
    result: dict[str, Any] = {"metric": name, "comparison": comparison,
                              "baseline": baseline, "candidate": candidate}
    if comparison == "exact":
        matches = [left == right for left, right in zip(baseline, candidate)]
        result.update({"matches": matches, "pass": all(matches)})
        return result

    baseline_numbers = [float(value) for value in baseline]
    candidate_numbers = [float(value) for value in candidate]
    result["baseline_cv"] = coefficient_of_variation(baseline_numbers)
    cv_limit = float(stats["baseline_cv_max"])
    if comparison == "no_increase":
        max_delta = float(policy.get("max_delta", 0))
        deltas = [c - b for b, c in zip(baseline_numbers, candidate_numbers)]
        result.update({"deltas": deltas, "max_delta": max_delta,
                       "pass": all(delta <= max_delta for delta in deltas)})
        return result

    point, upper, pair_ratios = paired_bootstrap_ratios(
        baseline_numbers,
        candidate_numbers,
        int(stats["bootstrap_resamples"]),
        int(stats["seed"]) + seed_offset,
        float(stats["one_sided_confidence"]),
    )
    max_ratio = float(policy["max_ratio"])
    improved = sum(ratio < 1.0 for ratio in pair_ratios)
    required_improved = int(
        policy.get(
            "min_improved_pairs",
            scenario.get("min_improved_pairs", max(1, len(pair_ratios) - 1)),
        )
    )
    passed = (
        result["baseline_cv"] <= cv_limit
        and point <= max_ratio
        and upper <= max_ratio
        and improved >= required_improved
    )
    if comparison == "latency":
        max_delta = float(policy["max_delta"])
        deltas = [c - b for b, c in zip(baseline_numbers, candidate_numbers)]
        passed = passed and statistics.fmean(deltas) <= max_delta and max(deltas) <= max_delta
        result.update({"deltas": deltas, "max_delta": max_delta})
    result.update({
        "point_ratio": point,
        "upper_ratio": upper,
        "max_ratio": max_ratio,
        "pair_ratios": pair_ratios,
        "improved_pairs": improved,
        "required_improved_pairs": required_improved,
        "baseline_cv_max": cv_limit,
        "pass": passed,
    })
    return result


def command_compare(args: argparse.Namespace) -> int:
    document, scenario_hash = load_scenarios(args.scenarios)
    scenario = find_scenario(document, args.scenario)
    evidence_root = args.host_manifest.resolve().parent
    if args.output_json.resolve().parent != evidence_root:
        raise ValueError("--output-json must be directly inside the host-manifest directory")
    if args.output_markdown.resolve().parent != evidence_root:
        raise ValueError("--output-markdown must be directly inside the host-manifest directory")
    render = args.scenario == "render_and_interaction"
    host_manifest = validate_host_manifest(
        args.host_manifest.resolve(), scenario, document, scenario_hash, render
    )
    host_os = normalized_os(host_manifest["host"]["system"])
    host_identity = {
        field: str(host_manifest["host"][field]) for field in HOST_IDENTITY_FIELDS
    }
    if len(args.baseline_run) != len(args.candidate_run):
        raise ValueError("--baseline-run and --candidate-run counts must match")
    if len(args.baseline_run) != int(scenario["pairs"]):
        raise ValueError(
            f"scenario {args.scenario} requires {scenario['pairs']} pairs, got {len(args.baseline_run)}"
        )
    expected_repeats = int(scenario.get("candidate_repeats", 0))
    if len(args.candidate_repeat_run) != expected_repeats:
        raise ValueError(
            f"scenario {args.scenario} requires {expected_repeats} candidate repeats, "
            f"got {len(args.candidate_repeat_run)}"
        )
    run_contracts, run_chronology = validate_run_contract_collection(
        evidence_root, args, scenario, scenario_hash, host_identity
    )

    inventory_paths: list[tuple[Path, str]] = []
    inventory_paths.append(
        (evidence_root / host_manifest["scenario_file"]["path"], "shared")
    )
    inventory_paths.append(
        (evidence_root / host_manifest["build_receipt"]["path"], "shared")
    )
    inventory_paths.extend(
        (Path(identity["path"]), "shared")
        for identity in host_manifest["binaries"].values()
    )
    seed_context: tuple[dict[str, Any], Path] | None = None
    if scenario["requires_mpv"]:
        seed_manifest_path = evidence_root / "seed-contract.json"
        seed_context = validate_seed_contract_manifest(
            seed_manifest_path, evidence_root, scenario, scenario_hash
        )
        inventory_paths.append((seed_manifest_path, "shared"))
        inventory_paths.extend(
            (item, "shared")
            for item in sorted(path for path in seed_context[1].rglob("*") if path.is_file())
        )

    all_run_paths = [
        path.resolve()
        for paths in (args.baseline_run, args.candidate_run, args.candidate_repeat_run)
        for path in paths
    ]
    if len(all_run_paths) != len(set(all_run_paths)):
        raise ValueError("run directories must be unique across baseline, candidate, and repeats")
    mpv_run_provenance: list[dict[str, Any]] = []
    for role, paths in (
        ("baseline", args.baseline_run),
        ("candidate", args.candidate_run),
        ("candidate", args.candidate_repeat_run),
    ):
        for path in paths:
            resolved = path.resolve()
            try:
                resolved.relative_to(evidence_root)
            except ValueError as error:
                raise ValueError(
                    f"run directory must stay inside evidence root {evidence_root}: {resolved}"
                ) from error
            validated_artifacts = validate_run_artifacts(
                resolved,
                role,
                scenario,
                document,
                scenario_hash,
                host_os,
                host_manifest,
                seed_context,
                run_contracts,
            )
            inventory_paths.extend((artifact, role) for artifact in validated_artifacts)
            if scenario["requires_mpv"]:
                mpv_run_provenance.append(
                    {
                        "run_ids": [
                            run_contracts[directory.resolve()]["run_id"]
                            for directory in process_run_directories(resolved, scenario)
                        ],
                        "role": role,
                        **measured_mpv_executable_provenance(resolved, scenario),
                    }
                )

    if mpv_run_provenance:
        executable_identities = {
            (
                item["executable"],
                item["executable_bytes"],
                item["executable_sha256"],
            )
            for item in mpv_run_provenance
        }
        if len(executable_identities) != 1:
            raise ValueError("paired/repeat runs used different mpv executable identities")

    if scenario["requires_mpv"]:
        fixture_manifest_path = evidence_root / "fixture" / "manifest.json"
        fixture_manifest = load_json_object(fixture_manifest_path)
        require_artifact_value(
            fixture_manifest_path,
            "schema",
            fixture_manifest.get("schema"),
            "ytt.tui-perf.fixture.v1",
        )
        fixture_path = Path(str(fixture_manifest.get("path", "")))
        if not fixture_path.is_file():
            raise ValueError(f"{fixture_manifest_path}: fixture no longer exists: {fixture_path}")
        require_artifact_value(
            fixture_manifest_path,
            "fixture SHA-256",
            sha256_file(fixture_path),
            fixture_manifest.get("sha256"),
        )
        for artifact_path, _role in inventory_paths:
            if artifact_path.name == "http-ready.json":
                require_artifact_value(
                    artifact_path,
                    "served fixture SHA-256",
                    load_json_object(artifact_path).get("fixture_sha256"),
                    fixture_manifest.get("sha256"),
                )
        inventory_paths.append((fixture_manifest_path, "shared"))
        inventory_paths.append((fixture_path, "shared"))

    baseline_runs = [load_run(path) for path in args.baseline_run]
    candidate_runs = [load_run(path) for path in args.candidate_run]
    candidate_repeat_runs = [load_run(path) for path in args.candidate_repeat_run]
    policies = scenario.get("metrics", {})
    results = []
    repeat_metrics = [dict() for _ in candidate_repeat_runs]
    for offset, (metric, policy) in enumerate(sorted(policies.items())):
        names = expand_metric_names(
            metric, baseline_runs + candidate_runs + candidate_repeat_runs
        )
        for variant_offset, name in enumerate(names):
            baseline = [run[name] for run in baseline_runs]
            candidate = [run[name] for run in candidate_runs]
            results.append(
                compare_metric(
                    name,
                    policy,
                    baseline,
                    candidate,
                    scenario,
                    document["statistics"],
                    offset * 100 + variant_offset,
                )
            )
            for repeat, run in zip(repeat_metrics, candidate_repeat_runs):
                repeat[name] = run[name]
    if not results:
        raise ValueError(f"scenario {args.scenario} produced no metric comparisons")

    seen_artifacts: dict[Path, str] = {}
    raw_artifacts = []
    for path, role in inventory_paths:
        resolved = path.resolve()
        if resolved in seen_artifacts:
            raise ValueError(
                f"raw artifact path collision: {resolved} is used as both "
                f"{seen_artifacts[resolved]!r} and {role!r}"
            )
        seen_artifacts[resolved] = role
        raw_artifacts.append(relative_artifact(resolved, evidence_root, role))
    raw_artifacts.sort(key=lambda item: (item["role"], item["path"]))

    def relative_run(path: Path) -> str:
        resolved = path.resolve()
        try:
            return resolved.relative_to(evidence_root).as_posix()
        except ValueError as error:
            raise ValueError(f"run path escapes evidence root: {resolved}") from error

    report = {
        "schema": SCHEMA,
        "kind": "paired_comparison",
        "scenario": args.scenario,
        "scenario_sha256": scenario_hash,
        "generated_unix_s": int(time.time()),
        "host": {"os": platform.system(), "release": platform.release(),
                 "machine": platform.machine(), "python": platform.python_version()},
        "baseline_runs": [relative_run(path) for path in args.baseline_run],
        "candidate_runs": [relative_run(path) for path in args.candidate_run],
        "candidate_repeat_runs": [relative_run(path) for path in args.candidate_repeat_run],
        "run_chronology": [
            {
                field: contract[field]
                for field in (
                    "run_id",
                    "kind",
                    "role",
                    "pair_index",
                    "pair_order",
                    "within_pair_ordinal",
                    "repeat_index",
                    "geometry_index",
                    "terminal_geometry",
                    "started_unix_ns",
                    "finished_unix_ns",
                    "started_monotonic_ns",
                    "finished_monotonic_ns",
                    "duration_ns",
                )
            }
            for contract in run_chronology
        ],
        "candidate_repeat_metrics": repeat_metrics,
        "measurement_scope": document["sampling"],
        "limitations": measurement_limitations(render),
        "evidence": {
            "host_manifest": relative_artifact(
                args.host_manifest.resolve(), evidence_root, "shared"
            ),
            "sources": host_manifest["sources"],
            "binaries": host_manifest["binaries"],
            "raw_artifacts": raw_artifacts,
            "raw_set_sha256": raw_inventory_digest(raw_artifacts),
            "mpv_executable_provenance": mpv_run_provenance,
            "mpv_null_audio_zero_volume_proven": (
                True if scenario["requires_mpv"] else None
            ),
        },
        "required_runtime_checklist": document.get("required_runtime_checklist", []),
        "runtime_checklist_status": "not_run_by_performance_harness",
        "metrics": results,
        "pass": all(result["pass"] for result in results),
    }
    atomic_json(args.output_json, report)
    args.output_markdown.parent.mkdir(parents=True, exist_ok=True)
    args.output_markdown.write_text(markdown_report(report), encoding="utf-8")
    print(json.dumps({"pass": report["pass"], "json": str(args.output_json),
                      "markdown": str(args.output_markdown), "scenario_sha256": scenario_hash}))
    return 0 if report["pass"] else 1


def markdown_report(report: dict[str, Any]) -> str:
    lines = [
        f"# TUI performance: `{report['scenario']}`",
        "",
        f"Overall: **{'PASS' if report['pass'] else 'FAIL'}**",
        "",
        f"Scenario SHA-256: `{report['scenario_sha256']}`",
        "",
        f"Host manifest SHA-256: `{report['evidence']['host_manifest']['sha256']}`",
        "",
        f"Raw artifact set SHA-256: `{report['evidence']['raw_set_sha256']}` "
        f"({len(report['evidence']['raw_artifacts'])} files)",
        "",
        f"Baseline source: `{report['evidence']['sources']['baseline']['head']}` "
        f"tree `{report['evidence']['sources']['baseline']['tree']}`",
        "",
        f"Candidate source: `{report['evidence']['sources']['candidate']['head']}` "
        f"tree `{report['evidence']['sources']['candidate']['tree']}`",
        "",
        f"Candidate diagnostic repeats: {len(report['candidate_repeat_runs'])}",
        "",
        "Measurement limitations:",
    ]
    lines.extend(f"- {item}" for item in report["limitations"])
    lines.extend([
        "",
        "Visual runtime checklist: **NOT RUN by this performance harness**",
    ])
    lines.extend(f"- [ ] {item}" for item in report["required_runtime_checklist"])
    lines.extend([
        "",
        "| Metric | Policy | Result | Detail |",
        "| --- | --- | --- | --- |",
    ])
    for metric in report["metrics"]:
        if metric["comparison"] in {"ratio", "latency"}:
            detail = (
                f"ratio {metric['point_ratio']:.4f}; upper {metric['upper_ratio']:.4f}; "
                f"limit {metric['max_ratio']:.4f}; CV {metric['baseline_cv']:.4f}; "
                f"improved {metric['improved_pairs']}/{len(metric['pair_ratios'])}"
            )
            if metric["comparison"] == "latency":
                detail += f"; deltas {metric['deltas']}; max +{metric['max_delta']}"
        elif metric["comparison"] == "no_increase":
            detail = f"deltas {metric['deltas']}; max allowed {metric['max_delta']}"
        else:
            detail = f"paired equality {metric['matches']}"
        lines.append(
            f"| `{metric['metric']}` | {metric['comparison']} | "
            f"**{'PASS' if metric['pass'] else 'FAIL'}** | {detail} |"
        )
    lines.extend(["", "Generated from paired native-host artifacts; raw paths are retained in JSON.", ""])
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    validate = sub.add_parser("validate", help="validate the versioned scenario file")
    validate.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    validate.set_defaults(handler=command_validate)

    scenario = sub.add_parser("scenario", help="print one scenario or a dotted scalar field")
    scenario.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    scenario.add_argument("--id", required=True)
    scenario.add_argument("--field")
    scenario.set_defaults(handler=command_scenario)

    traffic = sub.add_parser("traffic", help="print a traffic profile or one scalar field")
    traffic.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    traffic.add_argument("--name", required=True)
    traffic.add_argument("--field")
    traffic.set_defaults(handler=command_traffic)

    setting = sub.add_parser("setting", help="print a dotted top-level scenario setting")
    setting.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    setting.add_argument("--field", required=True)
    setting.set_defaults(handler=command_setting)

    build = sub.add_parser(
        "build", help="perform a fresh controlled source-bound release build"
    )
    build.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    build.add_argument("--scenario", required=True)
    build.add_argument("--baseline-root", type=Path, required=True)
    build.add_argument("--candidate-root", type=Path, required=True)
    build.add_argument("--output", type=Path, required=True)
    build.add_argument("--target-root", type=Path, required=True)
    build.set_defaults(handler=command_build)

    receipt = sub.add_parser("receipt", help="read one field from a controlled build receipt")
    receipt.add_argument("--receipt", type=Path, required=True)
    receipt.add_argument("--artifact", required=True)
    receipt.add_argument("--field", required=True)
    receipt.set_defaults(handler=command_receipt)

    path_preflight = sub.add_parser(
        "path-preflight",
        help="resolve a new evidence root and reject source/seed containment",
    )
    path_preflight.add_argument("--output-root", type=Path, required=True)
    path_preflight.add_argument(
        "--protected-root", type=Path, action="append", required=True
    )
    path_preflight.set_defaults(handler=command_path_preflight)

    seed_contract = sub.add_parser(
        "seed-contract", help="validate identical role seeds and create an immutable snapshot"
    )
    seed_contract.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    seed_contract.add_argument("--scenario", required=True)
    seed_contract.add_argument("--baseline-root", type=Path, required=True)
    seed_contract.add_argument("--candidate-root", type=Path, required=True)
    seed_contract.add_argument("--snapshot", type=Path, required=True)
    seed_contract.add_argument("--output", type=Path, required=True)
    seed_contract.set_defaults(handler=command_seed_contract)

    manifest = sub.add_parser("manifest", help="write OS, CPU, RAM, tool, and binary identity")
    manifest.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    manifest.add_argument("--scenario", required=True)
    manifest.add_argument("--output", type=Path, required=True)
    manifest.add_argument("--build-receipt", type=Path, required=True)
    manifest.set_defaults(handler=command_manifest)

    materialize = sub.add_parser(
        "materialize", help="replace fixture/home placeholders in an isolated seed tree"
    )
    materialize.add_argument("--root", type=Path, required=True)
    materialize.add_argument("--home", type=Path, required=True)
    materialize.add_argument("--fixture-url", required=True)
    materialize.add_argument(
        "--playlist-relative", type=Path, default=Path("fixture/tui-perf-stream.m3u")
    )
    materialize.add_argument("--manifest", type=Path, required=True)
    materialize.add_argument("--input-snapshot", type=Path, required=True)
    materialize.add_argument("--seed-label", default="unspecified")
    materialize.set_defaults(handler=command_materialize)

    launch_policy = sub.add_parser(
        "launch-policy", help="freeze background network/update work for a gating run"
    )
    launch_policy.add_argument("--root", type=Path, required=True)
    launch_policy.add_argument("--output", type=Path, required=True)
    launch_policy.set_defaults(handler=command_launch_policy)

    run_start = sub.add_parser(
        "run-start", help="atomically start one paired/repeat chronology contract"
    )
    run_start.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    run_start.add_argument("--scenario", required=True)
    run_start.add_argument("--output", type=Path, required=True)
    run_start.add_argument("--kind", choices=("paired", "candidate_repeat"), required=True)
    run_start.add_argument("--role", choices=("baseline", "candidate"), required=True)
    run_start.add_argument("--pair-index", type=int)
    run_start.add_argument("--repeat-index", type=int)
    run_start.add_argument("--geometry-index", type=int)
    run_start.add_argument("--width", type=int)
    run_start.add_argument("--height", type=int)
    run_start.set_defaults(handler=command_run_start)

    run_finish = sub.add_parser(
        "run-finish", help="atomically close one chronology contract after all producers exit"
    )
    run_finish.add_argument("--contract", type=Path, required=True)
    run_finish.set_defaults(handler=command_run_finish)

    cleanup = sub.add_parser(
        "cleanup", help="verify, terminate, wait for, and revalidate a live run identity"
    )
    cleanup.add_argument("--identity", type=Path, required=True)
    cleanup.add_argument("--timeout-secs", type=float, default=10.0)
    cleanup.set_defaults(handler=command_cleanup)

    fixture = sub.add_parser("fixture", help="create deterministic PCM WAV silence")
    fixture.add_argument("--output", type=Path, required=True)
    fixture.add_argument("--manifest", type=Path)
    fixture.add_argument("--seconds", type=float, default=900.0)
    fixture.add_argument("--sample-rate", type=int, default=8_000)
    fixture.set_defaults(handler=command_fixture)

    check = sub.add_parser("check", help="validate one sampler/control artifact set")
    check.add_argument("--samples", type=Path, required=True)
    check.add_argument("--control", type=Path)
    check.add_argument("--scenario-sha256")
    check.add_argument("--require-silent-mpv", action="store_true")
    check.add_argument("--require-observer-close", action="store_true")
    check.set_defaults(handler=command_check)

    serve = sub.add_parser("serve", help="serve the fixture with Range/throttle/outage controls")
    serve.add_argument("--file", type=Path, required=True)
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=0)
    serve.add_argument("--ready-file", type=Path)
    serve.add_argument("--request-log", type=Path, required=True)
    serve.add_argument("--run-id", required=True)
    serve.add_argument("--shutdown-token", required=True)
    serve.add_argument("--throttle-bps", type=int, default=0)
    serve.add_argument("--outage-every-bytes", type=int, default=0)
    serve.add_argument("--outage-ms", type=int, default=0)
    serve.add_argument("--disconnect-every-bytes", type=int, default=0)
    serve.add_argument("--verbose", action="store_true")
    serve.set_defaults(handler=command_serve)

    stop_server = sub.add_parser(
        "stop-server",
        help="authenticate, gracefully stop, and revalidate an exact fixture server",
    )
    stop_server.add_argument("--identity", type=Path, required=True)
    stop_server.add_argument("--expected-run-id", required=True)
    stop_server.add_argument("--timeout-secs", type=float, default=10.0)
    stop_server.set_defaults(handler=command_stop_server)

    compare = sub.add_parser("compare", help="compare paired run directories and write reports")
    compare.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    compare.add_argument("--scenario", required=True)
    compare.add_argument("--host-manifest", type=Path, required=True)
    compare.add_argument("--baseline-run", type=Path, action="append", required=True)
    compare.add_argument("--candidate-run", type=Path, action="append", required=True)
    compare.add_argument("--candidate-repeat-run", type=Path, action="append", default=[])
    compare.add_argument("--output-json", type=Path, required=True)
    compare.add_argument("--output-markdown", type=Path, required=True)
    compare.set_defaults(handler=command_compare)

    create_checksums = sub.add_parser(
        "create-checksums",
        help="create or overwrite and verify a portable SHA256SUMS inventory",
    )
    create_checksums.add_argument("--root", type=Path, required=True)
    create_checksums.add_argument("--output", type=Path, required=True)
    create_checksums.set_defaults(handler=command_create_checksums)
    verify = sub.add_parser(
        "verify-checksums", help="read-only transport verification of an existing SHA256SUMS"
    )
    verify.add_argument("--root", type=Path, required=True)
    verify.add_argument("--output", type=Path, required=True)
    verify.set_defaults(handler=command_verify_checksums)

    self_test = sub.add_parser("self-test", help="run deterministic statistics edge-case checks")
    self_test.set_defaults(handler=command_self_test)
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        return int(args.handler(args))
    except (ValueError, OSError, json.JSONDecodeError) as error:
        print(f"tui-perf.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
