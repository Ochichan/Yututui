#!/usr/bin/env python3
"""Create and verify reproducible evidence for the pinned mpv v0.32 compatibility build."""

from __future__ import annotations

import argparse
import faulthandler
import hashlib
import json
import os
import platform
import re
import secrets
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path
from typing import Any, Callable


OFFICIAL_REPOSITORY = "https://github.com/mpv-player/mpv.git"
PINNED_COMMIT = "70b991749df389bcc0a4e145b5687233a03b4ed7"
COMPAT_UPSTREAM_COMMIT = "1805681aaba22aa19a27ecfdb639c983d91f83e6"
COMPAT_PATCHED_FILES = (
    "options/m_config.c",
    "options/m_option.h",
    "options/m_property.c",
    "player/client.c",
    "player/command.c",
)
BUILD_SCHEMA = "ytt.mpv-032-build.v1"
PROBE_SCHEMA = "ytt.mpv-032-probe.v1"
IPC_MAX_LINE_BYTES = 1024 * 1024
IPC_PROCESS_POLL_SECS = 0.1
FFMPEG_VERSION = "4.4.8"
FFMPEG_PACKAGE_VERSIONS = {
    "libavcodec": "58.134.100",
    "libavfilter": "7.110.100",
    "libavformat": "58.76.100",
    "libavutil": "56.70.100",
    "libswresample": "3.9.100",
    "libswscale": "5.9.100",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_identity(path: Path) -> dict[str, Any]:
    resolved = path.resolve()
    if not resolved.is_file():
        raise ValueError(f"missing regular file: {resolved}")
    return {
        "path": str(resolved),
        "bytes": resolved.stat().st_size,
        "sha256": sha256_file(resolved),
    }


def private_file_identity(path: Path) -> dict[str, Any]:
    identity = file_identity(path)
    return {
        "name": path.name,
        "bytes": identity["bytes"],
        "sha256": identity["sha256"],
        "path_recorded": False,
    }


def resolve_executable(value: str) -> Path:
    candidate = Path(value).expanduser()
    if candidate.is_file():
        return candidate.resolve()
    discovered = shutil.which(value)
    if discovered is None:
        raise ValueError(f"executable is not available: {value}")
    return Path(discovered).resolve()


def atomic_json(path: Path, value: dict[str, Any]) -> None:
    if path.exists():
        raise ValueError(f"output must name a new path: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def checked(
    command: list[str],
    *,
    cwd: Path | None = None,
    timeout: float = 30.0,
    env: dict[str, str] | None = None,
) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
        env=env,
        check=False,
    )
    if completed.returncode != 0:
        raise ValueError(
            f"command failed ({completed.returncode}): {command!r}\n{completed.stderr.strip()}"
        )
    return completed.stdout.strip()


def checked_bytes(
    command: list[str], *, cwd: Path | None = None, timeout: float = 30.0
) -> bytes:
    completed = subprocess.run(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    if completed.returncode != 0:
        raise ValueError(
            f"command failed ({completed.returncode}): {command!r}\n"
            f"{completed.stderr.decode('utf-8', errors='replace').strip()}"
        )
    return completed.stdout


def sanitized_runtime_environment(binary: Path) -> dict[str, str]:
    """Keep Windows DLL resolution target-local instead of inheriting MSYS toolchain PATH."""
    if os.name != "nt":
        return dict(os.environ)
    system_root = os.environ.get("SystemRoot") or os.environ.get("WINDIR")
    if not system_root:
        raise ValueError("Windows runtime probe requires SystemRoot or WINDIR")
    system32 = Path(system_root) / "System32"
    environment = {
        "SystemRoot": system_root,
        "WINDIR": os.environ.get("WINDIR", system_root),
        "PATH": os.pathsep.join((str(binary.parent), str(system32))),
    }
    for key in ("COMSPEC", "PATHEXT"):
        if os.environ.get(key):
            environment[key] = os.environ[key]
    return environment


def require_equal(label: str, actual: Any, expected: Any) -> None:
    if actual != expected:
        raise ValueError(f"{label}: expected {expected!r}, observed {actual!r}")


def require_environment_executable(
    label: str, environment_value: str | None, executable: Path
) -> None:
    if not environment_value:
        raise ValueError(f"completed build did not record explicit {label}")
    normalized = environment_value.replace("\\", "/")
    resolved = str(executable).replace("\\", "/")
    if os.name == "nt" and normalized.startswith("/"):
        if not resolved.casefold().endswith(normalized.casefold()):
            raise ValueError(
                f"{label} identity: {environment_value!r} does not select {executable}"
            )
        return
    require_equal(f"{label} identity", Path(environment_value).resolve(), executable)


def require_environment_path(
    label: str, environment_value: str | None, expected: Path
) -> None:
    if not environment_value:
        raise ValueError(f"completed build did not record explicit {label}")
    normalized = environment_value.replace("\\", "/").rstrip("/")
    resolved = str(expected.resolve()).replace("\\", "/").rstrip("/")
    if os.name == "nt" and normalized.startswith("/"):
        if not resolved.casefold().endswith(normalized.casefold()):
            raise ValueError(
                f"{label} identity does not select the explicit dependency prefix"
            )
        return
    require_equal(f"{label} identity", Path(environment_value).resolve(), expected.resolve())


def msys_path(path: Path) -> str:
    resolved = str(path.resolve()).replace("\\", "/")
    match = re.fullmatch(r"([A-Za-z]):(/.*)", resolved)
    if match is None:
        raise ValueError(f"cannot translate Windows path for MSYS Python: {path}")
    return f"/{match.group(1).lower()}{match.group(2)}"


def warning_class(text: str) -> str:
    lowered = text.casefold()
    if "syntaxwarning" in lowered:
        return "python_syntax_warning"
    if "deprecationwarning" in lowered:
        return "python_deprecation_warning"
    if "deprecated" in lowered:
        return "deprecation_warning"
    if "warning:" in lowered:
        return "compiler_or_tool_warning"
    return "warning"


def collect_build_warnings(
    logs: dict[str, Path], replacements: dict[str, str]
) -> dict[str, Any]:
    variants: list[tuple[str, str]] = []
    for raw, token in replacements.items():
        variants.append((raw, token))
        variants.append((raw.replace("\\", "/"), token))
    variants.sort(key=lambda item: len(item[0]), reverse=True)
    entries: list[dict[str, Any]] = []
    for log_name, path in logs.items():
        for line_number, raw_line in enumerate(
            path.read_text(encoding="utf-8", errors="replace").splitlines(), start=1
        ):
            if re.search(r"(?i)\b(?:warning|deprecated|deprecationwarning|syntaxwarning)\b", raw_line) is None:
                continue
            normalized = raw_line.strip()
            for raw, token in variants:
                if raw:
                    normalized = re.sub(
                        re.escape(raw), token, normalized, flags=re.IGNORECASE
                    )
            normalized = normalized[:1000]
            entries.append(
                {
                    "log": log_name,
                    "line": line_number,
                    "class": warning_class(normalized),
                    "text": normalized,
                    "text_sha256": hashlib.sha256(
                        normalized.encode("utf-8")
                    ).hexdigest(),
                }
            )
    by_class: dict[str, int] = {}
    for entry in entries:
        category = str(entry["class"])
        by_class[category] = by_class.get(category, 0) + 1
    return {
        "policy": "warnings_are_non_ship_v1",
        "count": len(entries),
        "by_class": dict(sorted(by_class.items())),
        "entries": entries,
        "ship_evidence_eligible": not entries,
    }


def manifest_command(args: argparse.Namespace) -> int:
    source = args.source.resolve()
    prefix = args.prefix.resolve()
    binary = args.binary.resolve()
    git = resolve_executable(args.git)
    python = resolve_executable(args.python)
    waf_python = resolve_executable(args.waf_python)
    pkg_config = resolve_executable(args.pkg_config)
    require_equal("manifest/probe Python identity", Path(sys.executable).resolve(), python)
    waf_python_os_name = checked(
        [str(waf_python), "-c", "import os; print(os.name)"]
    )
    if binary.suffix.casefold() == ".exe":
        require_equal("Windows manifest/probe Python os.name", os.name, "nt")
        require_equal("Windows waf Python os.name", waf_python_os_name, "posix")
    cc = resolve_executable(args.cc)
    cxx = resolve_executable(args.cxx)
    compat_patch_input = args.compat_patch.absolute()
    if compat_patch_input.is_symlink() or not compat_patch_input.is_file():
        raise ValueError("compatibility patch must be a regular non-symlink file")
    compat_patch = compat_patch_input.resolve()
    ffmpeg_prefix_input = args.ffmpeg_prefix.absolute()
    if ffmpeg_prefix_input.is_symlink() or not ffmpeg_prefix_input.is_dir():
        raise ValueError("FFmpeg prefix must be a regular non-symlink directory")
    ffmpeg_prefix = ffmpeg_prefix_input.resolve()
    ffmpeg_pkg_config_path = ffmpeg_prefix / "lib" / "pkgconfig"
    if not ffmpeg_pkg_config_path.is_dir():
        raise ValueError("FFmpeg prefix is missing lib/pkgconfig")
    ffmpeg_source_archive = args.ffmpeg_source_archive.absolute()
    if ffmpeg_source_archive.is_symlink() or not ffmpeg_source_archive.is_file():
        raise ValueError("FFmpeg source archive must be a regular non-symlink file")
    require_equal(
        "FFmpeg source archive name",
        ffmpeg_source_archive.name,
        f"ffmpeg-{FFMPEG_VERSION}.tar.xz",
    )
    ffmpeg_logs = {
        "configure": args.ffmpeg_configure_log.absolute(),
        "build": args.ffmpeg_build_log.absolute(),
        "install": args.ffmpeg_install_log.absolute(),
    }
    for label, path in ffmpeg_logs.items():
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"FFmpeg {label} log must be a regular non-symlink file")
    ffmpeg_version_header = ffmpeg_prefix / "include" / "libavutil" / "ffversion.h"
    version_header_text = ffmpeg_version_header.read_text(
        encoding="utf-8", errors="strict"
    )
    if f'FFMPEG_VERSION "{FFMPEG_VERSION}"' not in version_header_text:
        raise ValueError("FFmpeg installed version header is not the required 4.4.8")
    expected_pc_files = sorted(f"{name}.pc" for name in FFMPEG_PACKAGE_VERSIONS)
    require_equal(
        "FFmpeg pkg-config module set",
        sorted(path.name for path in ffmpeg_pkg_config_path.glob("*.pc")),
        expected_pc_files,
    )
    pkg_config_environment = dict(os.environ)
    pkg_config_environment["PKG_CONFIG_PATH"] = str(ffmpeg_pkg_config_path)
    ffmpeg_package_versions = {
        name: checked(
            [str(pkg_config), "--modversion", name], env=pkg_config_environment
        )
        for name in FFMPEG_PACKAGE_VERSIONS
    }
    require_equal(
        "FFmpeg pkg-config versions",
        ffmpeg_package_versions,
        FFMPEG_PACKAGE_VERSIONS,
    )
    require_equal(
        "compatibility patch upstream commit",
        args.compat_upstream_commit,
        COMPAT_UPSTREAM_COMMIT,
    )
    if not source.is_dir() or not prefix.is_dir():
        raise ValueError("source and prefix must be existing directories")
    commit = checked([str(git), "rev-parse", "HEAD"], cwd=source)
    require_equal("mpv source commit", commit, PINNED_COMMIT)
    tree = checked([str(git), "show", "-s", "--format=%T", "HEAD"], cwd=source)
    remote = checked([str(git), "remote", "get-url", "origin"], cwd=source)
    require_equal("mpv origin", remote, OFFICIAL_REPOSITORY)
    patched_files = checked(
        [str(git), "diff", "--name-only", "HEAD", "--"], cwd=source
    )
    require_equal(
        "compatibility-patched mpv source files",
        patched_files.splitlines(),
        list(COMPAT_PATCHED_FILES),
    )
    patch_numstat = checked([str(git), "apply", "--numstat", str(compat_patch)])
    require_equal(
        "compatibility patch file list",
        [line.split("\t", 2)[-1] for line in patch_numstat.splitlines()],
        list(COMPAT_PATCHED_FILES),
    )
    checked(
        [str(git), "apply", "--reverse", "--check", "--whitespace=error-all", str(compat_patch)],
        cwd=source,
    )
    checked([str(git), "diff", "--check"], cwd=source)
    patched_diff = checked_bytes([str(git), "diff", "--binary", "HEAD", "--"], cwd=source)
    if not patched_diff:
        raise ValueError("compatibility-patched source produced an empty diff")
    commit_timestamp = int(
        checked([str(git), "show", "-s", "--format=%ct", "HEAD"], cwd=source)
    )
    runtime_environment = sanitized_runtime_environment(binary)
    version_output = checked(
        [str(binary), "--no-config", "--version"], env=runtime_environment
    )
    first_line = version_output.splitlines()[0] if version_output else ""
    # A shallow detached checkout has no local tag, so mpv's generated version can be the
    # pinned release commit abbreviation instead of the human tag. The full source commit was
    # verified above; accept either truthful rendering of that same v0.32.0 release identity.
    release_commit_version = re.escape(PINNED_COMMIT[:7])
    if re.match(rf"^mpv (?:v?0\.32\.0|{release_commit_version})(?:[-\s]|$)", first_line) is None:
        raise ValueError(f"built executable is not the pinned mpv 0.32.0 release: {first_line!r}")

    log_paths = {
        "clone": args.clone_log,
        "bootstrap": args.bootstrap_log,
        "configure": args.configure_log,
        "build": args.build_log,
        "install": args.install_log,
    }
    log_identities = {name: file_identity(path) for name, path in log_paths.items()}
    warning_replacements = {
        str(ffmpeg_source_archive.resolve().parent.parent): "$DEPENDENCY_WORK_ROOT",
        str(source.parent): "$OUTPUT_ROOT",
        str(source): "$MPV_SOURCE",
        str(prefix): "$MPV_PREFIX",
        str(ffmpeg_prefix): "$FFMPEG_PREFIX",
    }
    if os.name == "nt":
        warning_replacements.update(
            {
                msys_path(source.parent): "$OUTPUT_ROOT",
                msys_path(ffmpeg_source_archive.resolve().parent.parent): "$DEPENDENCY_WORK_ROOT",
                msys_path(source): "$MPV_SOURCE",
                msys_path(prefix): "$MPV_PREFIX",
                msys_path(ffmpeg_prefix): "$FFMPEG_PREFIX",
            }
        )
    warnings = collect_build_warnings(
        {
            **log_paths,
            **{f"ffmpeg_{name}": path for name, path in ffmpeg_logs.items()},
        },
        warning_replacements,
    )
    waf = source / "waf"
    waf_argument = (
        msys_path(waf)
        if os.name == "nt" and waf_python_os_name == "posix"
        else str(waf)
    )
    waf_version = checked([str(waf_python), waf_argument, "--version"], cwd=source)
    if not (waf_version.startswith("waf 2.0.9 ") or waf_version.startswith("waf 2.0.27 ")):
        raise ValueError(f"unsupported waf version in completed build: {waf_version!r}")
    bootstrap_script = (
        msys_path(source / "bootstrap.py")
        if os.name == "nt" and waf_python_os_name == "posix"
        else str(source / "bootstrap.py")
    )
    bootstrap_command = [str(waf_python), bootstrap_script]
    waf_seed = None
    if args.waf_seed is not None:
        waf_seed = file_identity(args.waf_seed)
        bootstrap_command = ["copy-prebuilt-waf", waf_seed["path"], str(waf)]
    runtime_dependencies = []
    seen_runtime_names: set[str] = set()
    for runtime_source in args.runtime_dll:
        source_identity = file_identity(runtime_source)
        name_key = runtime_source.name.casefold()
        if name_key in seen_runtime_names:
            raise ValueError(f"duplicate runtime DLL basename: {runtime_source.name}")
        seen_runtime_names.add(name_key)
        staged = binary.parent / runtime_source.name
        staged_identity = file_identity(staged)
        require_equal(
            f"staged runtime DLL {runtime_source.name} bytes",
            staged_identity["bytes"],
            source_identity["bytes"],
        )
        require_equal(
            f"staged runtime DLL {runtime_source.name} SHA-256",
            staged_identity["sha256"],
            source_identity["sha256"],
        )
        runtime_dependencies.append(
            {"name": runtime_source.name, "source": source_identity, "staged": staged_identity}
        )
    runtime_dependencies.sort(key=lambda item: str(item["name"]).casefold())
    if os.name == "nt":
        require_equal(
            "explicit Windows runtime DLL set",
            {name.casefold() for name in seen_runtime_names},
            {"libwinpthread-1.dll", "zlib1.dll"},
        )
    commands = {
        "source_acquisition": [
            [str(git), "init", "-q", str(source)],
            [str(git), "-C", str(source), "remote", "add", "origin", OFFICIAL_REPOSITORY],
            [
                str(git),
                "-C",
                str(source),
                "fetch",
                "--depth=1",
                "origin",
                PINNED_COMMIT,
            ],
            [
                str(git),
                "-c",
                "advice.detachedHead=false",
                "-C",
                str(source),
                "checkout",
                "-q",
                "--detach",
                "FETCH_HEAD",
            ],
        ],
        "compatibility_patch": [
            [str(git), "-C", str(source), "apply", "--check", "--whitespace=error-all", str(compat_patch)],
            [str(git), "-C", str(source), "apply", "--whitespace=error-all", str(compat_patch)],
            [str(git), "-C", str(source), "diff", "--check"],
        ],
        "bootstrap": bootstrap_command,
        "configure": [
            str(waf_python),
            waf_argument,
            "configure",
            f"--prefix={prefix}",
            *args.configure_arg,
        ],
        "build": [str(waf_python), waf_argument, "build", f"-j{args.jobs}"],
        "install": [str(waf_python), waf_argument, "install"],
    }
    environment = {
        key: os.environ.get(key)
        for key in (
            "LC_ALL",
            "LANG",
            "SOURCE_DATE_EPOCH",
            "PYTHONHASHSEED",
            "CC",
            "CXX",
            "CFLAGS",
            "CPPFLAGS",
            "LDFLAGS",
            "PKG_CONFIG_PATH",
            "MSYSTEM",
            "MINGW_PREFIX",
        )
    }
    missing_empty_build_flags = [
        key
        for key, expected in (
            ("CFLAGS", args.cflags),
            ("CPPFLAGS", args.cppflags),
            ("LDFLAGS", args.ldflags),
        )
        if expected == "" and environment[key] is None
    ]
    for key in missing_empty_build_flags:
        # MSYS-to-native Windows process creation omits empty environment entries.
        # The exact empty values remain independently bound by required CLI arguments.
        environment[key] = ""
    require_equal("SOURCE_DATE_EPOCH", environment["SOURCE_DATE_EPOCH"], str(commit_timestamp))
    require_equal("PYTHONHASHSEED", environment["PYTHONHASHSEED"], "0")
    require_equal("LC_ALL", environment["LC_ALL"], "C")
    require_environment_executable("CC", environment["CC"], cc)
    require_environment_executable("CXX", environment["CXX"], cxx)
    require_equal("CFLAGS", environment["CFLAGS"], args.cflags)
    require_equal("CPPFLAGS", environment["CPPFLAGS"], args.cppflags)
    require_equal("LDFLAGS", environment["LDFLAGS"], args.ldflags)
    require_environment_path(
        "PKG_CONFIG_PATH", environment["PKG_CONFIG_PATH"], ffmpeg_pkg_config_path
    )
    environment["PKG_CONFIG_PATH"] = "$FFMPEG_PREFIX/lib/pkgconfig"
    if "-DNDEBUG" not in (args.cflags.split() + args.cppflags.split()):
        raise ValueError("the supported mpv 0.32 build must define NDEBUG")
    document = {
        "schema": BUILD_SCHEMA,
        "repository_identity": {
            "kind": "official_mpv_git",
            "url_sha256": hashlib.sha256(OFFICIAL_REPOSITORY.encode("utf-8")).hexdigest(),
            "url_recorded": False,
        },
        "pinned_commit": PINNED_COMMIT,
        "source": {
            "path": str(source),
            "commit": commit,
            "tree": tree,
            "commit_timestamp": commit_timestamp,
            "origin_sha256": hashlib.sha256(remote.encode("utf-8")).hexdigest(),
            "origin_recorded": False,
            "tracked_worktree_clean": False,
            "patched_source": True,
            "patched_files": list(COMPAT_PATCHED_FILES),
            "patched_diff_bytes": len(patched_diff),
            "patched_diff_sha256": hashlib.sha256(patched_diff).hexdigest(),
        },
        "compatibility_patch": {
            "kind": "upstream_mpv_backport_v1",
            "upstream_commit": COMPAT_UPSTREAM_COMMIT,
            "patch": file_identity(compat_patch),
            "patched_files": list(COMPAT_PATCHED_FILES),
            "reverse_apply_check": True,
            "diff_check": True,
        },
        "prefix": str(prefix),
        "binary": file_identity(binary),
        "version_output": version_output,
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python_implementation": platform.python_implementation(),
        },
        "commands": commands,
        "build_working_directory": str(source),
        "environment": environment,
        "build_flags": {
            "provenance": "build-mpv-032-wrapper-explicit-v1",
            "cflags": args.cflags,
            "cppflags": args.cppflags,
            "ldflags": args.ldflags,
            "ndebug_required": True,
            "missing_empty_environment_values_normalized": missing_empty_build_flags,
        },
        "runtime_dependencies": {
            "policy": "explicit_verified_target_local_ucrt_v1",
            "sanitized_probe_path": runtime_environment["PATH"],
            "dependencies": runtime_dependencies,
        },
        "ffmpeg_dependency": {
            "policy": "explicit_prepared_ffmpeg_4_4_8_prefix_v1",
            "version": FFMPEG_VERSION,
            "prefix": {
                "path": "$FFMPEG_PREFIX",
                "path_recorded": False,
                "pkg_config_path": "$FFMPEG_PREFIX/lib/pkgconfig",
            },
            "packages": ffmpeg_package_versions,
            "pkg_config": {
                **private_file_identity(pkg_config),
                "version": checked([str(pkg_config), "--version"]),
            },
            "source_archive": private_file_identity(ffmpeg_source_archive),
            "installed_version_header": private_file_identity(ffmpeg_version_header),
            "build_receipt": {
                name: private_file_identity(path) for name, path in ffmpeg_logs.items()
            },
        },
        "warnings": warnings,
        "ship_evidence_eligible": warnings["ship_evidence_eligible"],
        "tools": {
            "git": {
                **file_identity(git),
                "version": checked([str(git), "--version"]),
            },
            "python": {
                **file_identity(python),
                "version": checked([str(python), "--version"]),
            },
            "waf_python": {
                **file_identity(waf_python),
                "version": checked([str(waf_python), "--version"]),
            },
            "waf": {**file_identity(waf), "version": waf_version},
            "cc": {
                **file_identity(cc),
                "version": checked([str(cc), "--version"]),
            },
            "cxx": {
                **file_identity(cxx),
                "version": checked([str(cxx), "--version"]),
            },
        },
        "python_roles": {
            "manifest_and_ipc_probe": {
                "path": str(python),
                "os_name": os.name,
                "matches_running_interpreter": True,
            },
            "waf_only": {
                "path": str(waf_python),
                "os_name": waf_python_os_name,
            },
        },
        "waf_seed": waf_seed,
        "logs": log_identities,
        "global_install_performed": False,
        "target_local_prefix": True,
    }
    atomic_json(args.output.resolve(), document)
    print(json.dumps({"ok": True, "output": str(args.output.resolve())}))
    return 0


def load_object(path: Path) -> dict[str, Any]:
    def reject_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key {key!r} in {path}")
            result[key] = value
        return result

    value = json.loads(path.read_text(encoding="utf-8-sig"), object_pairs_hook=reject_duplicates)
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


class IpcClient:
    def __init__(
        self,
        send_line: Callable[[bytes], None],
        receive_line: Callable[[float], bytes],
        close_transport: Callable[[], None],
        process: subprocess.Popen[bytes],
    ):
        self.send_line = send_line
        self.receive_line = receive_line
        self.close_transport = close_transport
        self.process = process
        self.events: list[dict[str, Any]] = []
        self.request_id = 0

    def request(self, command: list[Any], timeout: float = 5.0) -> dict[str, Any]:
        self.request_id += 1
        request_id = self.request_id
        payload = json.dumps(
            {"command": command, "request_id": request_id}, separators=(",", ":")
        ).encode("utf-8") + b"\n"
        self.send_line(payload)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ValueError(f"mpv IPC request timed out: {command!r}")
            return_code = self.process.poll()
            if return_code is not None:
                raise ValueError(
                    f"mpv exited during IPC request with code {return_code}: {command!r}"
                )
            try:
                line = self.receive_line(min(remaining, IPC_PROCESS_POLL_SECS))
            except TimeoutError:
                continue
            if not line:
                raise ValueError("mpv IPC stream closed")
            if len(line) > IPC_MAX_LINE_BYTES:
                raise ValueError(
                    f"mpv IPC line exceeds {IPC_MAX_LINE_BYTES} bytes"
                )
            try:
                item = json.loads(line.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ValueError(f"mpv IPC returned invalid JSON: {error}") from error
            if not isinstance(item, dict):
                raise ValueError("mpv IPC returned a non-object JSON value")
            if item.get("request_id") == request_id:
                return item
            self.events.append(item)

    def close(self) -> None:
        self.close_transport()


def open_windows_named_pipe(
    endpoint: str,
) -> tuple[Callable[[bytes], None], Callable[[float], bytes], Callable[[], None]]:
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    create_file = kernel32.CreateFileW
    create_file.argtypes = (
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    )
    create_file.restype = wintypes.HANDLE
    peek_named_pipe = kernel32.PeekNamedPipe
    peek_named_pipe.argtypes = (
        wintypes.HANDLE,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    )
    peek_named_pipe.restype = wintypes.BOOL
    read_file = kernel32.ReadFile
    read_file.argtypes = (
        wintypes.HANDLE,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    )
    read_file.restype = wintypes.BOOL
    write_file = kernel32.WriteFile
    write_file.argtypes = (
        wintypes.HANDLE,
        wintypes.LPCVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    )
    write_file.restype = wintypes.BOOL
    close_handle = kernel32.CloseHandle
    close_handle.argtypes = (wintypes.HANDLE,)
    close_handle.restype = wintypes.BOOL

    handle = create_file(
        endpoint,
        0x80000000 | 0x40000000,  # GENERIC_READ | GENERIC_WRITE
        0,
        None,
        3,  # OPEN_EXISTING
        0,
        None,
    )
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    pending = bytearray()
    closed = False

    def send_line(payload: bytes) -> None:
        if closed:
            raise ValueError("mpv IPC transport is closed")
        offset = 0
        while offset < len(payload):
            chunk = payload[offset:]
            buffer = ctypes.create_string_buffer(chunk)
            written = wintypes.DWORD()
            if not write_file(handle, buffer, len(chunk), ctypes.byref(written), None):
                raise ctypes.WinError(ctypes.get_last_error())
            if written.value <= 0:
                raise ValueError("mpv IPC write made no progress")
            offset += written.value

    def receive_line(timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while True:
            newline = pending.find(b"\n")
            if newline >= 0:
                line = bytes(pending[: newline + 1])
                del pending[: newline + 1]
                return line
            if len(pending) > IPC_MAX_LINE_BYTES:
                raise ValueError(f"mpv IPC line exceeds {IPC_MAX_LINE_BYTES} bytes")
            available = wintypes.DWORD()
            if not peek_named_pipe(
                handle, None, 0, None, ctypes.byref(available), None
            ):
                error = ctypes.get_last_error()
                if error in {109, 232}:  # ERROR_BROKEN_PIPE | ERROR_NO_DATA
                    return b""
                raise ctypes.WinError(error)
            if available.value:
                size = min(available.value, 64 * 1024)
                buffer = ctypes.create_string_buffer(size)
                read = wintypes.DWORD()
                if not read_file(handle, buffer, size, ctypes.byref(read), None):
                    error = ctypes.get_last_error()
                    if error in {109, 232}:
                        return b""
                    raise ctypes.WinError(error)
                if read.value <= 0:
                    return b""
                pending.extend(buffer.raw[: read.value])
                if len(pending) > IPC_MAX_LINE_BYTES and b"\n" not in pending:
                    raise ValueError(
                        f"mpv IPC line exceeds {IPC_MAX_LINE_BYTES} bytes"
                    )
                continue
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for mpv IPC response")
            time.sleep(min(0.01, remaining))

    def close_transport() -> None:
        nonlocal closed
        if not closed:
            closed = True
            if not close_handle(handle):
                error = ctypes.get_last_error()
                if error != 6:  # ERROR_INVALID_HANDLE
                    raise ctypes.WinError(error)

    return send_line, receive_line, close_transport


def open_unix_socket(
    endpoint: str,
) -> tuple[Callable[[bytes], None], Callable[[float], bytes], Callable[[], None]]:
    transport = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    transport.connect(endpoint)
    pending = bytearray()

    def send_line(payload: bytes) -> None:
        transport.sendall(payload)

    def receive_line(timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while True:
            newline = pending.find(b"\n")
            if newline >= 0:
                line = bytes(pending[: newline + 1])
                del pending[: newline + 1]
                return line
            if len(pending) > IPC_MAX_LINE_BYTES:
                raise ValueError(f"mpv IPC line exceeds {IPC_MAX_LINE_BYTES} bytes")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for mpv IPC response")
            ready, _write, _error = select.select([transport], [], [], remaining)
            if not ready:
                raise TimeoutError("timed out waiting for mpv IPC response")
            chunk = transport.recv(64 * 1024)
            if not chunk:
                return b""
            pending.extend(chunk)
            if len(pending) > IPC_MAX_LINE_BYTES and b"\n" not in pending:
                raise ValueError(f"mpv IPC line exceeds {IPC_MAX_LINE_BYTES} bytes")

    return send_line, receive_line, transport.close


def connect_ipc(endpoint: str, process: subprocess.Popen[bytes], timeout: float) -> IpcClient:
    deadline = time.monotonic() + timeout
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            detail = (stderr or stdout).decode("utf-8", errors="replace").strip()
            suffix = f": {detail}" if detail else ""
            raise ValueError(
                f"mpv exited before IPC readiness with code {process.returncode}{suffix}"
            )
        try:
            if os.name == "nt":
                send_line, receive_line, close_transport = open_windows_named_pipe(endpoint)
            else:
                send_line, receive_line, close_transport = open_unix_socket(endpoint)
            return IpcClient(send_line, receive_line, close_transport, process)
        except OSError as error:
            last_error = error
            time.sleep(0.02)
    raise ValueError(f"timed out connecting to mpv IPC {endpoint}: {last_error}")


def terminate_process_tree(process: subprocess.Popen[bytes], timeout: float = 5.0) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        system_root = os.environ.get("SystemRoot") or os.environ.get("WINDIR")
        taskkill = (
            Path(system_root).resolve() / "System32" / "taskkill.exe"
            if system_root
            else None
        )
        if taskkill is not None and taskkill.is_file():
            subprocess.run(
                [str(taskkill), "/PID", str(process.pid), "/T", "/F"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process.poll() is None:
        process.kill()
    process.wait(timeout=timeout)


def finish_probe_process(
    process: subprocess.Popen[bytes], *, graceful: bool, timeout: float = 5.0
) -> None:
    if graceful:
        try:
            process.wait(timeout=timeout)
            return
        except subprocess.TimeoutExpired:
            pass
    terminate_process_tree(process, timeout)


def response_data(client: IpcClient, command: list[Any]) -> tuple[bool, Any, str]:
    response = client.request(command)
    error = response.get("error")
    return error == "success", response.get("data"), str(error)


def named_capabilities(values: Any, *, object_names: bool = False) -> set[str]:
    if not isinstance(values, list):
        return set()
    if object_names:
        return {
            str(item["name"])
            for item in values
            if isinstance(item, dict) and isinstance(item.get("name"), str)
        }
    return {str(item) for item in values if isinstance(item, str)}


def probe_command(args: argparse.Namespace) -> int:
    faulthandler.dump_traceback_later(max(5.0, args.timeout_secs * 4.0), repeat=True)
    build_manifest = load_object(args.build_manifest.resolve())
    require_equal("build schema", build_manifest.get("schema"), BUILD_SCHEMA)
    require_equal("build commit", build_manifest.get("pinned_commit"), PINNED_COMMIT)
    binary = Path(str(build_manifest.get("binary", {}).get("path", ""))).resolve()
    require_equal("build binary identity", build_manifest.get("binary"), file_identity(binary))
    if args.binary is not None:
        require_equal("requested probe binary", args.binary.resolve(), binary)

    with tempfile.TemporaryDirectory(prefix="ytt-mpv032-probe-") as temporary:
        temporary_root = Path(temporary)
        cache_root = temporary_root / "cache"
        cache_root.mkdir()
        private_home = temporary_root / "home"
        runtime_root = temporary_root / "runtime"
        temp_root = temporary_root / "tmp"
        for directory in (private_home, runtime_root, temp_root):
            directory.mkdir()
        private_app_data = private_home / "AppData" / "Roaming"
        private_local_app_data = private_home / "AppData" / "Local"
        private_app_data.mkdir(parents=True)
        private_local_app_data.mkdir(parents=True)
        fixture = temporary_root / "probe.wav"
        with wave.open(str(fixture), "wb") as stream:
            stream.setnchannels(1)
            stream.setsampwidth(2)
            stream.setframerate(8_000)
            stream.writeframes(b"\0\0" * 16_000)
        if os.name == "nt":
            endpoint = rf"\\.\pipe\ytt-mpv032-{os.getpid()}-{secrets.token_hex(8)}"
        else:
            endpoint = str(temporary_root / "mpv.sock")
        launch_command = [
            str(binary),
            "--no-config",
            "--idle=yes",
            "--pause=yes",
            "--cache=yes",
            "--cache-on-disk=no",
            f"--cache-dir={cache_root}",
            "--cache-unlink-files=immediate",
            "--demuxer-max-bytes=33554432",
            "--demuxer-max-back-bytes=8388608",
            "--ao=null",
            "--vo=null",
            "--audio-display=no",
            "--terminal=yes",
            "--msg-level=all=warn",
            f"--input-ipc-server={endpoint}",
        ]
        probe_environment = {
            key: os.environ[key]
            for key in (
                "LD_LIBRARY_PATH",
                "DYLD_LIBRARY_PATH",
            )
            if key in os.environ and os.environ[key]
        }
        probe_environment.update(sanitized_runtime_environment(binary))
        probe_environment.update(
            {
                "HOME": str(private_home),
                "USERPROFILE": str(private_home),
                "APPDATA": str(private_app_data),
                "LOCALAPPDATA": str(private_local_app_data),
                "XDG_CONFIG_HOME": str(private_home / ".config"),
                "XDG_DATA_HOME": str(private_home / ".local" / "share"),
                "XDG_CACHE_HOME": str(private_home / ".cache"),
                "XDG_STATE_HOME": str(private_home / ".local" / "state"),
                "XDG_RUNTIME_DIR": str(runtime_root),
                "TEMP": str(temp_root),
                "TMP": str(temp_root),
                "TMPDIR": str(temp_root),
                "LANG": "C",
                "LC_ALL": "C",
            }
        )
        process = subprocess.Popen(
            launch_command,
            env=probe_environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            creationflags=(0x08000000 | 0x00000200) if os.name == "nt" else 0,
            start_new_session=os.name != "nt",
        )
        client: IpcClient | None = None
        document: dict[str, Any] | None = None
        try:
            client = connect_ipc(endpoint, process, args.timeout_secs)
            query_results: dict[str, dict[str, Any]] = {}
            for property_name in ("mpv-version", "property-list", "command-list"):
                ok, data, error = response_data(client, ["get_property", property_name])
                query_results[property_name] = {"ok": ok, "data": data, "error": error}
            properties = named_capabilities(query_results["property-list"]["data"])
            commands = named_capabilities(
                query_results["command-list"]["data"], object_names=True
            )
            required_options = {
                "cache-on-disk",
                "cache-dir",
                "cache-unlink-files",
                "demuxer-max-back-bytes",
                "demuxer-max-bytes",
            }
            optional_options = {
                "cache-pause-wait",
                "demuxer-cache-dir",
                "demuxer-cache-unlink-files",
                "demuxer-readahead-secs",
            }
            option_queries: dict[str, dict[str, Any]] = {}
            for option_name in sorted(required_options | optional_options):
                ok, data, error = response_data(
                    client, ["get_property", f"option-info/{option_name}"]
                )
                option_queries[option_name] = {"ok": ok, "data": data, "error": error}
            options = {
                name for name, result in option_queries.items() if result["ok"]
            }
            query_results["option-info"] = option_queries
            observed_properties = ("paused-for-cache", "time-pos")
            subscriptions: dict[str, dict[str, Any]] = {}
            for index, property_name in enumerate(observed_properties, start=1):
                ok, data, error = response_data(
                    client, ["observe_property", 32_000 + index, property_name]
                )
                subscriptions[property_name] = {"ok": ok, "data": data, "error": error}

            runtime_checks: dict[str, dict[str, Any]] = {}
            runtime_failures: list[str] = []

            def check_success(label: str, command: list[Any]) -> tuple[bool, Any]:
                ok, data, error = response_data(client, command)
                runtime_checks[label] = {
                    "command": command,
                    "ok": ok,
                    "data": data,
                    "error": error,
                }
                if not ok:
                    runtime_failures.append(label)
                return ok, data

            loaded, _data = check_success(
                "load_fixture", ["loadfile", str(fixture), "replace"]
            )
            readiness_deadline = time.monotonic() + args.timeout_secs
            duration: Any = None
            readiness_error = "loadfile failed"
            fixture_ready = False
            while loaded and time.monotonic() < readiness_deadline:
                ok, duration, readiness_error = response_data(
                    client, ["get_property", "duration"]
                )
                if (
                    ok
                    and not isinstance(duration, bool)
                    and isinstance(duration, (int, float))
                    and duration > 0
                ):
                    fixture_ready = True
                    break
                if readiness_error not in {"success", "property unavailable"}:
                    break
                time.sleep(0.02)
            runtime_checks["fixture_ready"] = {
                "ok": fixture_ready,
                "duration": duration,
                "error": None if fixture_ready else readiness_error,
                "fixture": file_identity(fixture),
            }
            if not fixture_ready:
                runtime_failures.append("fixture_ready")
            check_success("enable_cache_on_disk", ["set_property", "cache-on-disk", True])
            enabled_ok, enabled = check_success(
                "read_enabled_cache_on_disk", ["get_property", "cache-on-disk"]
            )
            if enabled_ok and enabled is not True:
                runtime_failures.append("read_enabled_cache_on_disk")
                runtime_checks["read_enabled_cache_on_disk"]["ok"] = False
            check_success("interactive_fast_seek", ["seek", 0.5, "absolute+keyframes"])
            check_success("exact_seek", ["seek", 1.5, "absolute+exact"])
            state_ok, state = check_success(
                "read_demuxer_cache_state", ["get_property", "demuxer-cache-state"]
            )
            required_state_members = ("file-cache-bytes",)
            optional_state_members = ("raw-input-rate",)
            state_members: dict[str, Any] = {}
            if state_ok and isinstance(state, dict):
                state_members = {
                    name: state[name]
                    for name in (*required_state_members, *optional_state_members)
                    if name in state
                }
            missing_state_members = sorted(
                set(required_state_members) - set(state_members)
            )
            runtime_checks["demuxer_cache_state_members"] = {
                "ok": state_ok
                and isinstance(state, dict)
                and not missing_state_members,
                "required": list(required_state_members),
                "optional": list(optional_state_members),
                "observed": state_members,
                "missing": missing_state_members,
            }
            if not runtime_checks["demuxer_cache_state_members"]["ok"]:
                runtime_failures.append("demuxer_cache_state_members")
            check_success("disable_cache_on_disk", ["set_property", "cache-on-disk", False])
            disabled_ok, disabled = check_success(
                "read_disabled_cache_on_disk", ["get_property", "cache-on-disk"]
            )
            if disabled_ok and disabled is not False:
                runtime_failures.append("read_disabled_cache_on_disk")
                runtime_checks["read_disabled_cache_on_disk"]["ok"] = False
            check_success("stop", ["stop"])

            required_commands = {"loadfile", "seek", "set", "stop", "quit"}
            required_properties = {
                "mpv-version",
                "property-list",
                "command-list",
                "paused-for-cache",
                "time-pos",
                "duration",
                "demuxer-cache-state",
            }
            optional_properties = {
                "cache-on-disk",
                "cache-speed",
                "demuxer-via-network",
                "seeking",
                "seekable",
                "partially-seekable",
                "seekable-ranges",
            }
            missing = {
                "commands": sorted(required_commands - commands),
                "properties": sorted(required_properties - properties),
                "options": sorted(required_options - options),
                "subscriptions": sorted(
                    name for name, result in subscriptions.items() if not result["ok"]
                ),
                "runtime_checks": sorted(set(runtime_failures)),
            }
            quit_ok, quit_data, quit_error = response_data(client, ["quit"])
            if not quit_ok:
                raise ValueError(f"mpv quit command failed: {quit_error}")
            document = {
                "schema": PROBE_SCHEMA,
                "pinned_commit": PINNED_COMMIT,
                "build_manifest": {
                    "path": str(args.build_manifest.resolve()),
                    "sha256": sha256_file(args.build_manifest.resolve()),
                },
                "binary": file_identity(binary),
                "launch_command": launch_command,
                "controlled_io": {
                    "stdin": "null",
                    "audio": "ao=null",
                    "video": "vo=null",
                    "terminal_output": "captured stdout/stderr",
                    "user_config": "disabled",
                    "cache_root": str(cache_root),
                    "cache_unlink": "immediate",
                    "environment_policy": "explicit_private_roots_sanitized_runtime_path_v2",
                    "environment_keys": sorted(probe_environment),
                    "private_home": str(private_home),
                    "runtime_root": str(runtime_root),
                    "temp_root": str(temp_root),
                },
                "query_results": query_results,
                "subscriptions": subscriptions,
                "runtime_checks": runtime_checks,
                "capabilities": {
                    "commands": sorted(commands),
                    "properties": sorted(properties),
                    "options": sorted(options),
                    "optional_property_support": {
                        name: name in properties for name in sorted(optional_properties)
                    },
                    "demuxer_cache_state_members": state_members,
                    "demuxer_cache_state_member_support": {
                        name: name in state_members
                        for name in (*required_state_members, *optional_state_members)
                    },
                },
                "required_missing": missing,
                "compatible": not any(missing.values()),
                "events": client.events,
                "quit": {"ok": quit_ok, "data": quit_data, "error": quit_error},
            }
        finally:
            try:
                if client is not None:
                    client.close()
            finally:
                finish_probe_process(process, graceful=document is not None)
        if document is None:
            raise ValueError("mpv probe did not produce a result")
        if process.returncode != 0:
            raise ValueError(f"mpv exited with code {process.returncode} after quit")
        stdout, stderr = process.communicate()
        document["process_exit_code"] = process.returncode
        document["process_output"] = {
            "stdout": stdout.decode("utf-8", errors="replace"),
            "stderr": stderr.decode("utf-8", errors="replace"),
        }
        atomic_json(args.output.resolve(), document)
        if not document["compatible"]:
            print(json.dumps(document["required_missing"], sort_keys=True), file=sys.stderr)
            return 1
    faulthandler.cancel_dump_traceback_later()
    print(json.dumps({"ok": True, "output": str(args.output.resolve())}))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    manifest = sub.add_parser("manifest", help="verify and manifest a completed pinned build")
    manifest.add_argument("--source", type=Path, required=True)
    manifest.add_argument("--prefix", type=Path, required=True)
    manifest.add_argument("--binary", type=Path, required=True)
    manifest.add_argument("--output", type=Path, required=True)
    manifest.add_argument("--clone-log", type=Path, required=True)
    manifest.add_argument("--bootstrap-log", type=Path, required=True)
    manifest.add_argument("--configure-log", type=Path, required=True)
    manifest.add_argument("--build-log", type=Path, required=True)
    manifest.add_argument("--install-log", type=Path, required=True)
    manifest.add_argument("--git", default="git")
    manifest.add_argument("--python", default=sys.executable)
    manifest.add_argument("--waf-python", required=True)
    manifest.add_argument("--cc", required=True)
    manifest.add_argument("--cxx", required=True)
    manifest.add_argument("--pkg-config", required=True)
    manifest.add_argument("--jobs", type=int, required=True)
    manifest.add_argument("--cflags", required=True)
    manifest.add_argument("--cppflags", required=True)
    manifest.add_argument("--ldflags", required=True)
    manifest.add_argument("--compat-patch", type=Path, required=True)
    manifest.add_argument("--compat-upstream-commit", required=True)
    manifest.add_argument("--ffmpeg-prefix", type=Path, required=True)
    manifest.add_argument("--ffmpeg-source-archive", type=Path, required=True)
    manifest.add_argument("--ffmpeg-configure-log", type=Path, required=True)
    manifest.add_argument("--ffmpeg-build-log", type=Path, required=True)
    manifest.add_argument("--ffmpeg-install-log", type=Path, required=True)
    manifest.add_argument("--runtime-dll", action="append", type=Path, default=[])
    manifest.add_argument("--configure-arg", action="append", default=[])
    manifest.add_argument("--waf-seed", type=Path)
    manifest.set_defaults(handler=manifest_command)
    probe = sub.add_parser("probe", help="launch the pinned binary and probe IPC capabilities")
    probe.add_argument("--build-manifest", type=Path, required=True)
    probe.add_argument("--binary", type=Path)
    probe.add_argument("--output", type=Path, required=True)
    probe.add_argument("--timeout-secs", type=float, default=15.0)
    probe.set_defaults(handler=probe_command)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    if getattr(args, "jobs", 1) <= 0:
        raise ValueError("--jobs must be positive")
    if getattr(args, "timeout_secs", 1.0) <= 0:
        raise ValueError("--timeout-secs must be positive")
    return int(args.handler(args))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"probe-mpv-032.py: {error}", file=sys.stderr)
        raise SystemExit(2)
