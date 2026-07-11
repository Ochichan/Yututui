#!/usr/bin/env python3
"""Dependency-free orchestration/reporting helpers for the ytt TUI perf matrix.

The Rust examples own native process and render measurements. This script owns the portable
parts: deterministic silence fixtures, a Range-capable constrained HTTP server, scenario-file
validation, paired fixed-seed bootstrap statistics, and merged JSON/Markdown reports.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import ipaddress
import json
import math
import os
import platform
import random
import shutil
import statistics
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


class DuplicateJsonKeyError(ValueError):
    pass


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


def sha256_tree(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(item for item in root.rglob("*") if item.is_file()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    return digest.hexdigest()


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
    raw = run_git(root, "ls-files", "-co", "--exclude-standard", "-z", binary=True)
    assert isinstance(raw, bytes)
    relative_paths = sorted(
        (item.decode("utf-8", errors="surrogateescape") for item in raw.split(b"\0") if item),
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
            with path.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
        else:
            digest.update(b"M")
    return digest.hexdigest(), len(relative_paths)


def source_identity(root: Path, build_command: str) -> dict[str, Any]:
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
    return {
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
        "build_command": build_command,
    }


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


def checksum_targets(root: Path, output: Path) -> list[Path]:
    root = root.resolve()
    output = output.resolve()
    return sorted(
        (
            path
            for path in root.rglob("*")
            if path.is_file()
            and path.resolve() != output
            and path.name != output.name + ".tmp"
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


def command_checksums(args: argparse.Namespace) -> int:
    count = write_checksums(args.root, args.output)
    verified = verify_checksums(args.root, args.output)
    if verified != count:
        raise ValueError("checksum write/verify count mismatch")
    print(json.dumps({"ok": True, "output": str(args.output.resolve()), "files": count}))
    return 0


def load_scenarios(path: Path) -> tuple[dict[str, Any], str]:
    try:
        raw = path.read_bytes()
        document = json.loads(raw, object_pairs_hook=reject_duplicate_json_keys)
    except (OSError, json.JSONDecodeError, DuplicateJsonKeyError) as error:
        raise ValueError(f"cannot read scenario file {path}: {error}") from error
    validate_scenarios(document)
    return document, hashlib.sha256(raw).hexdigest()


def validate_scenarios(document: dict[str, Any]) -> None:
    if document.get("schema") != "ytt.tui-perf.scenarios.v1":
        raise ValueError("scenario schema must be ytt.tui-perf.scenarios.v1")
    if document.get("version") != 1:
        raise ValueError("scenario version must be 1")
    stats = document.get("statistics")
    if not isinstance(stats, dict) or int(stats.get("bootstrap_resamples", 0)) < 1:
        raise ValueError("statistics.bootstrap_resamples must be positive")
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
        if not isinstance(value, (int, float)) or isinstance(value, bool) or value <= 0:
            raise ValueError(f"fixture.{field} must be positive")
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
        repeats = scenario.get("candidate_repeats")
        if not isinstance(repeats, int) or isinstance(repeats, bool) or repeats < 0:
            raise ValueError(f"{name}.candidate_repeats must be a non-negative integer")
        for field in ("warmup_s", "sample_s"):
            value = scenario.get(field)
            if (
                not isinstance(value, (int, float))
                or isinstance(value, bool)
                or not math.isfinite(value)
                or value < 0
            ):
                raise ValueError(f"{name}.{field} must be finite and non-negative")
        geometry = scenario.get("geometry")
        if not (
            isinstance(geometry, list)
            and geometry
            and all(
                isinstance(item, list)
                and len(item) == 2
                and all(isinstance(value, int) and value > 0 for value in item)
                for item in geometry
            )
        ):
            raise ValueError(f"{name}.geometry must contain positive [width,height] pairs")
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
                    isinstance(value, (int, float))
                    and not isinstance(value, bool)
                    and math.isfinite(value)
                    and value >= 0
                    for value in seeks
                )
            ):
                raise ValueError(f"{name}.seeks_s must contain finite non-negative numbers")
        if requires_mpv:
            margin = float(fixture["margin_s"])
            observation_end = float(scenario["warmup_s"]) + float(scenario["sample_s"])
            furthest_seek = max((float(value) for value in scenario.get("seeks_s", [])), default=0.0)
            required_duration = max(observation_end, furthest_seek) + margin
            if float(fixture["duration_s"]) < required_duration:
                raise ValueError(
                    f"fixture.duration_s must be at least {required_duration:g} for {name}"
                )
        metrics = scenario.get("metrics", {})
        if not isinstance(metrics, dict):
            raise ValueError(f"{name}.metrics must be an object")
        for metric, policy in metrics.items():
            if not isinstance(metric, str) or not isinstance(policy, dict):
                raise ValueError(f"{name}.metrics entries must be object policies")
            comparison = policy.get("comparison", "ratio")
            if comparison not in {"ratio", "latency", "no_increase", "exact"}:
                raise ValueError(f"{name}.{metric}: unsupported comparison {comparison!r}")
            if comparison in {"ratio", "latency"} and not isinstance(
                policy.get("max_ratio"), (int, float)
            ):
                raise ValueError(f"{name}.{metric}: ratio policy needs max_ratio")
            if comparison == "latency" and not isinstance(policy.get("max_delta"), (int, float)):
                raise ValueError(f"{name}.{metric}: latency policy needs max_delta")


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


def labeled_specs(specs: list[str], option: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for spec in specs:
        label, separator, value = spec.partition("=")
        if not separator or not label or not value:
            raise ValueError(f"{option} must use LABEL=VALUE")
        if label in values:
            raise ValueError(f"{option} repeats label {label!r}")
        values[label] = value
    return values


def command_manifest(args: argparse.Namespace) -> int:
    _, scenario_hash = load_scenarios(args.scenarios)
    binaries: dict[str, Any] = {}
    for label, raw_path in labeled_specs(args.binary, "--binary").items():
        path = Path(raw_path)
        if not path.is_file():
            raise ValueError(f"manifest binary does not exist: {path}")
        binaries[label] = {
            "path": str(path.resolve()),
            "bytes": path.stat().st_size,
            "sha256": sha256_file(path),
        }
    source_roots = labeled_specs(args.source_root, "--source-root")
    build_commands = labeled_specs(args.build_command, "--build-command")
    required_sources = {"baseline", "candidate"}
    if set(source_roots) != required_sources:
        raise ValueError("--source-root requires exactly baseline=PATH and candidate=PATH")
    if set(build_commands) != required_sources:
        raise ValueError(
            "--build-command requires exactly baseline=COMMAND and candidate=COMMAND"
        )
    evidence_root = args.output.resolve().parent
    for source_root in source_roots.values():
        require_ignored_evidence_root(Path(source_root), evidence_root)
    sources = {
        label: source_identity(Path(source_roots[label]), build_commands[label])
        for label in sorted(required_sources)
    }
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
            "path": str(args.scenarios.resolve()),
            "bytes": args.scenarios.stat().st_size,
            "sha256": scenario_hash,
        },
        "generated_unix_s": int(time.time()),
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "version": platform.version(),
            "machine": platform.machine(),
            "cpu_model": cpu_model(),
            "logical_cpu_count": os.cpu_count(),
            "total_memory_bytes": total_memory_bytes(),
        },
        "tools": {name: tool_version(command) for name, command in tool_commands.items()},
        "binaries": binaries,
        "sources": sources,
        "note": "actual mpv argv and executable are recorded in each sampler artifact",
    }
    atomic_json(args.output, manifest)
    print(json.dumps({"ok": True, "output": str(args.output), "scenario_sha256": scenario_hash}))
    return 0


def command_materialize(args: argparse.Namespace) -> int:
    if not args.root.is_dir():
        raise ValueError(f"seed root does not exist: {args.root}")
    root = args.root.resolve()
    home = args.home.resolve()
    seed_tree_sha256 = sha256_tree(root)
    cache_policy = seed_cache_policy(root)
    playlist = (root / args.playlist_relative).resolve()
    try:
        playlist.relative_to(root)
    except ValueError as error:
        raise ValueError("--playlist-relative must stay inside --root") from error

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
    if playlist_references == 0:
        raise ValueError(
            "seed JSON must reference {{TUI_PERF_PLAYLIST}} as the resumed Song.local_path"
        )

    atomic_text(
        playlist,
        f"#EXTM3U\n#EXTINF:-1,ytt deterministic performance fixture\n{args.fixture_url}\n",
    )
    changed = [str(playlist)]
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
        changed.append(str(path))
    manifest = {
        "schema": "ytt.tui-perf.materialize.v1",
        "changed": changed,
        "fixture_url": args.fixture_url,
        "fixture_host": str(fixture_ip),
        "playlist": str(playlist),
        "playback_target_mode": "local_m3u_indirection",
        "external_dns_required": False,
        "playlist_references": playlist_references,
        "seed_label": args.seed_label,
        "seed_tree_sha256": seed_tree_sha256,
        "seed_cache_policy": cache_policy,
    }
    if args.manifest:
        atomic_json(args.manifest, manifest)
    print(json.dumps(manifest, sort_keys=True))
    return 0


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
    print(json.dumps({"ok": True, "measured_samples": len(measured)}))
    return 0


class FixtureServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int], handler: type[http.server.BaseHTTPRequestHandler],
                 file: Path, throttle_bps: int, outage_every_bytes: int, outage_ms: int,
                 disconnect_every_bytes: int):
        super().__init__(address, handler)
        self.fixture_file = file
        self.throttle_bps = throttle_bps
        self.outage_every_bytes = outage_every_bytes
        self.outage_ms = outage_ms
        self.disconnect_every_bytes = disconnect_every_bytes
        self.transfer_lock = threading.Lock()
        self.total_transferred = 0
        self.next_outage = outage_every_bytes if outage_every_bytes else 0
        self.next_disconnect = disconnect_every_bytes if disconnect_every_bytes else 0


class RangeFixtureHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    @property
    def perf_server(self) -> FixtureServer:
        return self.server  # type: ignore[return-value]

    def do_HEAD(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        self._serve(send_body=False)

    def do_GET(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        self._serve(send_body=True)

    def _serve(self, send_body: bool) -> None:
        server = self.perf_server
        if self.path.split("?", 1)[0] not in {"/fixture.wav", "/" + server.fixture_file.name}:
            self.send_error(404)
            return
        size = server.fixture_file.stat().st_size
        try:
            start, end, partial = parse_range(self.headers.get("Range"), size)
        except ValueError:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        length = end - start + 1
        self.send_response(206 if partial else 200)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(length))
        self.send_header("ETag", '"ytt-perf-silence-v1"')
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if not send_body:
            return

        remaining = length
        with server.fixture_file.open("rb") as stream:
            stream.seek(start)
            while remaining:
                chunk = stream.read(min(64 * 1024, remaining))
                if not chunk:
                    return
                action = transfer_action(server, len(chunk))
                if action == "outage":
                    time.sleep(server.outage_ms / 1000.0)
                elif action == "disconnect":
                    self.close_connection = True
                    return
                started = time.monotonic()
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return
                remaining -= len(chunk)
                if server.throttle_bps:
                    target = len(chunk) / server.throttle_bps
                    delay = target - (time.monotonic() - started)
                    if delay > 0:
                        time.sleep(delay)

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


def transfer_action(server: FixtureServer, count: int) -> str | None:
    with server.transfer_lock:
        server.total_transferred += count
        if server.next_disconnect and server.total_transferred >= server.next_disconnect:
            while server.next_disconnect <= server.total_transferred:
                server.next_disconnect += server.disconnect_every_bytes
            return "disconnect"
        if server.next_outage and server.total_transferred >= server.next_outage:
            while server.next_outage <= server.total_transferred:
                server.next_outage += server.outage_every_bytes
            return "outage"
    return None


def command_serve(args: argparse.Namespace) -> int:
    if not args.file.is_file():
        raise ValueError(f"fixture does not exist: {args.file}")
    for name in ("throttle_bps", "outage_every_bytes", "outage_ms", "disconnect_every_bytes"):
        if getattr(args, name) < 0:
            raise ValueError(f"--{name.replace('_', '-')} must be non-negative")
    server = FixtureServer(
        (args.host, args.port),
        RangeFixtureHandler,
        args.file.resolve(),
        args.throttle_bps,
        args.outage_every_bytes,
        args.outage_ms,
        args.disconnect_every_bytes,
    )
    server.verbose = args.verbose  # type: ignore[attr-defined]
    host, port = server.server_address[:2]
    manifest = {
        "schema": "ytt.tui-perf.http.v1",
        "pid": os.getpid(),
        "host": host,
        "port": port,
        "url": f"http://{host}:{port}/fixture.wav",
        "fixture_sha256": sha256_file(args.file),
        "throttle_bps": args.throttle_bps,
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
        case_p95 = case.get("p95_draw_ns")
        if (
            not isinstance(case_p95, (int, float))
            or isinstance(case_p95, bool)
            or not math.isfinite(float(case_p95))
            or float(case_p95) < 0
        ):
            raise ValueError(f"{path}: render case {case.get('name')} has invalid case p95")

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
                operations.setdefault(str(record.get("operation")), []).append(float(record["latency_ms"]))
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
    scenario_hash: str,
    render: bool,
) -> dict[str, Any]:
    manifest = load_json_object(path)
    require_artifact_value(path, "schema", manifest.get("schema"), "ytt.tui-perf.host.v1")
    require_artifact_value(path, "scenario", manifest.get("scenario"), scenario["id"])
    require_artifact_value(path, "scenario_sha256", manifest.get("scenario_sha256"), scenario_hash)
    host = manifest.get("host")
    if not isinstance(host, dict):
        raise ValueError(f"{path}: host must be an object")
    require_artifact_value(
        path,
        "host system",
        normalized_os(host.get("system")),
        normalized_os(platform.system()),
    )

    expected_binaries = (
        {"baseline_render", "candidate_render"}
        if render
        else {"baseline_ytt", "candidate_ytt", "sampler", "controller"}
    )
    binaries = manifest.get("binaries")
    binary_labels = set(binaries) if isinstance(binaries, dict) else set()
    if not isinstance(binaries, dict) or not expected_binaries.issubset(binary_labels):
        missing = sorted(expected_binaries - binary_labels)
        raise ValueError(f"{path}: missing required binary identities: {missing}")
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

    sources = manifest.get("sources")
    if not isinstance(sources, dict) or set(sources) != {"baseline", "candidate"}:
        raise ValueError(f"{path}: sources must contain exactly baseline and candidate")
    for label in ("baseline", "candidate"):
        recorded = sources[label]
        if not isinstance(recorded, dict):
            raise ValueError(f"{path}: source identity {label!r} must be an object")
        build_command = recorded.get("build_command")
        if not isinstance(build_command, str) or not build_command.strip():
            raise ValueError(f"{path}: source {label!r} has no build command")
        current = source_identity(Path(str(recorded.get("root", ""))), build_command)
        require_artifact_value(path, f"{label} source identity", current, recorded)
    return manifest


def validate_render_document(
    document: dict[str, Any],
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
) -> None:
    require_artifact_value(path, "schema", document.get("schema"), "ytt.tui-perf.render.v1")
    require_artifact_value(path, "kind", document.get("kind"), "render_summary")
    require_artifact_value(path, "scenario SHA-256", document.get("scenario_sha256"), scenario_hash)
    require_artifact_value(path, "OS", normalized_os(document.get("os")), host_os)
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


def process_run_directories(path: Path, scenario: dict[str, Any]) -> list[Path]:
    geometry = scenario["geometry"]
    if len(geometry) == 1:
        unexpected = sorted(item.name for item in path.glob("geometry-*") if item.is_dir())
        if unexpected:
            raise ValueError(f"{path}: single-geometry run has unexpected directories {unexpected}")
        return [path]
    expected = {f"geometry-{width}x{height}" for width, height in geometry}
    actual = {item.name for item in path.glob("geometry-*") if item.is_dir()}
    if actual != expected:
        raise ValueError(
            f"{path}: geometry directories are {sorted(actual)}, expected {sorted(expected)}"
        )
    return [path / name for name in sorted(expected)]


def validate_process_directory(
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_document: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
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
    if len(headers) != 1 or len(summaries) != 1 or not measured:
        raise ValueError(f"{samples_path}: incomplete sampler header, summary, or measurements")
    header = headers[0]
    require_artifact_value(samples_path, "schema", header.get("schema"), "ytt.tui-perf.samples.v1")
    require_artifact_value(samples_path, "scenario SHA-256", header.get("scenario_sha256"), scenario_hash)
    require_artifact_value(samples_path, "OS", normalized_os(header.get("os")), host_os)
    require_artifact_value(
        samples_path,
        "executed binary SHA-256",
        header.get("binary_sha256"),
        manifest["binaries"][f"{role}_ytt"]["sha256"],
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
    if any(record.get("kind") == "error" for record in samples):
        raise ValueError(f"{samples_path}: sampler recorded an error")

    artifacts = [samples_path]
    if scenario["controller"]:
        control_path = path / "control.ndjson"
        if not control_path.is_file():
            raise ValueError(f"{path}: missing control.ndjson")
        control = read_ndjson(control_path)
        control_headers = [record for record in control if record.get("kind") == "header"]
        control_summaries = [record for record in control if record.get("kind") == "summary"]
        if len(control_headers) != 1 or len(control_summaries) != 1:
            raise ValueError(f"{control_path}: incomplete controller header or summary")
        control_header = control_headers[0]
        require_artifact_value(control_path, "schema", control_header.get("schema"), "ytt.tui-perf.control.v1")
        require_artifact_value(control_path, "scenario SHA-256", control_header.get("scenario_sha256"), scenario_hash)
        require_artifact_value(control_path, "OS", normalized_os(control_header.get("os")), host_os)
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
        operations = [record.get("operation") for record in control if record.get("kind") == "operation"]
        expected_load = scenario.get("controller_load")
        expected_load_operation = {"resume-session": "resume_session", "none": "ready"}.get(expected_load)
        if expected_load_operation is None or operations.count(expected_load_operation) != 1:
            raise ValueError(f"{control_path}: controller load operation does not match {expected_load!r}")
        require_artifact_value(
            control_path,
            "seek operation count",
            operations.count("seek"),
            len(scenario.get("seeks_s", [])),
        )
        expected_pause_count = 1 if scenario["pause_policy"] == "pause-resume" else 0
        require_artifact_value(control_path, "pause operation count", operations.count("pause"), expected_pause_count)
        require_artifact_value(control_path, "resume operation count", operations.count("resume"), expected_pause_count)
        artifacts.append(control_path)

    if scenario["requires_mpv"]:
        materialize_path = path / "materialize.json"
        http_path = path / "http-ready.json"
        for support in (materialize_path, http_path):
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
        http = load_json_object(http_path)
        require_artifact_value(http_path, "schema", http.get("schema"), "ytt.tui-perf.http.v1")
        require_artifact_value(http_path, "loopback binding", http.get("bind_is_loopback"), True)
        require_artifact_value(http_path, "playback target", http.get("playback_target_mode"), "local_m3u_indirection")
        require_artifact_value(http_path, "external DNS", http.get("external_dns_required"), False)
        profile = scenario_document["traffic_profiles"][scenario["traffic_profile"]]
        for field in ("throttle_bps", "outage_every_bytes", "outage_ms", "disconnect_every_bytes"):
            require_artifact_value(http_path, field, http.get(field), profile[field])
        artifacts.extend((materialize_path, http_path))
    return artifacts


def validate_run_artifacts(
    path: Path,
    role: str,
    scenario: dict[str, Any],
    scenario_document: dict[str, Any],
    scenario_hash: str,
    host_os: str,
    manifest: dict[str, Any],
) -> list[Path]:
    if scenario["id"] == "render_and_interaction":
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
        )
        return [render_path]
    artifacts: list[Path] = []
    for directory in process_run_directories(path, scenario):
        artifacts.extend(
            validate_process_directory(
                directory,
                role,
                scenario,
                scenario_document,
                scenario_hash,
                host_os,
                manifest,
            )
        )
    return artifacts


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
    for entry in sorted(entries, key=lambda item: item["path"]):
        encoded = entry["path"].encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(bytes.fromhex(entry["sha256"]))
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
    point = geometric_mean_ratio(pair_ratios)
    rng = random.Random(seed)
    count = len(pair_ratios)
    sampled = []
    for _ in range(resamples):
        sampled.append(
            geometric_mean_ratio([pair_ratios[rng.randrange(count)] for _ in range(count)])
        )
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


def command_self_test(_args: argparse.Namespace) -> int:
    point, upper, ratios = paired_bootstrap_ratios([0.0, 0.0], [0.0, 0.0], 100, 7, 0.95)
    assert point == 1.0 and upper == 1.0 and ratios == [1.0, 1.0]
    point, upper, ratios = paired_bootstrap_ratios([1.0, 2.0], [0.0, 0.0], 100, 7, 0.95)
    assert point == 0.0 and upper == 0.0 and ratios == [0.0, 0.0]
    point, upper, ratios = paired_bootstrap_ratios([0.0, 1.0], [0.1, 1.0], 100, 7, 0.95)
    assert point == RATIO_INFINITY and upper == RATIO_INFINITY
    assert ratios == [RATIO_INFINITY, 1.0]
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
        "mean_draw_ns": 10,
        "p95_draw_ns": 999,
        "allocations": 200,
        "allocated_bytes": 400,
        "retained_bytes_delta": 0,
        "peak_live_bytes_delta": 2,
    }
    render_document = {
        "schema": "ytt.tui-perf.render.v1",
        "cases": [{
            "name": "pooled",
            "update_path": "app_update_msg_key",
            "measured_draws": 400,
            "p95_draw_ns": 123,
            "batches": [render_batch, render_batch],
            "buffer_style_digest": "buffer",
            "hit_map_digest": "hits",
        }],
    }
    render_metrics = render_metrics_from_document(render_document, Path("<self-test>"))
    assert render_metrics["render.pooled.p95_draw_ns"] == 123
    assert render_metrics["render.pooled.p95_reducer_input_to_draw_ns"] == 123
    render_document["cases"][0]["measured_draws"] = 399
    try:
        render_metrics_from_document(render_document, Path("<self-test>"))
    except ValueError:
        pass
    else:
        raise AssertionError("render measured_draws mismatch must be rejected")
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
        "os": normalized_os(platform.system()),
        "batches_per_case": 1,
        "draws_per_batch": 3,
        "cases": [
            {
                "name": "identity",
                "warmup_draws": 2,
                "measured_draws": 3,
                "batches": [{"draws": 3}],
            }
        ],
    }
    validate_render_document(
        identity_document,
        Path("<identity-self-test>"),
        "baseline",
        identity_scenario,
        "scenario",
        normalized_os(platform.system()),
        identity_manifest,
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
        )
    except ValueError:
        pass
    else:
        raise AssertionError("render scenario hash tampering must be rejected")
    with tempfile.TemporaryDirectory(prefix="ytt-perf-self-test-") as temporary:
        root = Path(temporary)
        raw = root / "raw.json"
        sums = root / "SHA256SUMS"
        raw.write_text('{"value":1}\n', encoding="utf-8")
        assert write_checksums(root, sums) == 1
        assert verify_checksums(root, sums) == 1
        raw.write_text('{"value":2}\n', encoding="utf-8")
        try:
            verify_checksums(root, sums)
        except ValueError:
            pass
        else:
            raise AssertionError("raw artifact tampering must be rejected")
    scenario_document, _ = load_scenarios(DEFAULT_SCENARIOS)
    soak = find_scenario(scenario_document, "memory_soak")
    assert soak["pause_policy"] == "none" and soak["pause_hold_ms"] == 0
    for wrapper_name in ("tui-perf.sh", "tui-perf.ps1"):
        wrapper = Path(__file__).with_name(wrapper_name).read_text(encoding="utf-8")
        for token in ("pause_policy", "pause_hold_ms", "--pause-hold-ms", "--no-pause"):
            # PowerShell uses camelCase locals but still queries the exact snake_case key.
            assert token in wrapper
    print(
        json.dumps(
            {
                "ok": True,
                "zero_ratio_cases": 3,
                "geometry_variant_cases": 2,
                "duplicate_key_cases": 1,
                "aggregate_render_p95_cases": 2,
                "render_identity_tamper_cases": 2,
                "checksum_tamper_cases": 1,
                "steady_soak_pause_cases": 1,
                "wrapper_pause_parity_cases": 2,
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
        args.host_manifest.resolve(), scenario, scenario_hash, render
    )
    host_os = normalized_os(host_manifest["host"]["system"])
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

    inventory_paths: list[tuple[Path, str]] = []
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
            inventory_paths.extend(
                (artifact, role)
                for artifact in validate_run_artifacts(
                    resolved,
                    role,
                    scenario,
                    document,
                    scenario_hash,
                    host_os,
                    host_manifest,
                )
            )

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

    seen_artifacts: set[Path] = set()
    raw_artifacts = []
    for path, role in inventory_paths:
        resolved = path.resolve()
        if resolved in seen_artifacts:
            continue
        seen_artifacts.add(resolved)
        raw_artifacts.append(relative_artifact(resolved, evidence_root, role))
    raw_artifacts.sort(key=lambda item: item["path"])

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
        "candidate_repeat_metrics": repeat_metrics,
        "evidence": {
            "host_manifest": relative_artifact(
                args.host_manifest.resolve(), evidence_root, "shared"
            ),
            "sources": host_manifest["sources"],
            "binaries": host_manifest["binaries"],
            "raw_artifacts": raw_artifacts,
            "raw_set_sha256": raw_inventory_digest(raw_artifacts),
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
        "Visual runtime checklist: **NOT RUN by this performance harness**",
    ]
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

    manifest = sub.add_parser("manifest", help="write OS, CPU, RAM, tool, and binary identity")
    manifest.add_argument("--scenarios", type=Path, default=DEFAULT_SCENARIOS)
    manifest.add_argument("--scenario", required=True)
    manifest.add_argument("--output", type=Path, required=True)
    manifest.add_argument("--binary", action="append", default=[])
    manifest.add_argument("--source-root", action="append", default=[])
    manifest.add_argument("--build-command", action="append", default=[])
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
    materialize.add_argument("--manifest", type=Path)
    materialize.add_argument("--seed-label", default="unspecified")
    materialize.set_defaults(handler=command_materialize)

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
    serve.add_argument("--throttle-bps", type=int, default=0)
    serve.add_argument("--outage-every-bytes", type=int, default=0)
    serve.add_argument("--outage-ms", type=int, default=0)
    serve.add_argument("--disconnect-every-bytes", type=int, default=0)
    serve.add_argument("--verbose", action="store_true")
    serve.set_defaults(handler=command_serve)

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

    checksums = sub.add_parser(
        "checksums", help="write and immediately verify a portable SHA256SUMS inventory"
    )
    checksums.add_argument("--root", type=Path, required=True)
    checksums.add_argument("--output", type=Path, required=True)
    checksums.set_defaults(handler=command_checksums)

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
