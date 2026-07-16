#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo metadata --format-version 1 --no-deps | python3 -c '
import json
import os
import sys

root = os.path.realpath(sys.argv[1])
metadata = json.load(sys.stdin)
manifests = {
    package["id"]: os.path.relpath(os.path.realpath(package["manifest_path"]), root)
    for package in metadata["packages"]
}
packages = {manifests[package["id"]]: package for package in metadata["packages"]}
members = {manifests[package_id] for package_id in metadata["workspace_members"]}
default_members = {
    manifests[package_id] for package_id in metadata["workspace_default_members"]
}

expected_members = {"Cargo.toml", "crates/yututui-core/Cargo.toml"}
if members != expected_members:
    raise SystemExit(
        f"workspace members drifted: expected {sorted(expected_members)}, got {sorted(members)}"
    )
if default_members != {"Cargo.toml"}:
    raise SystemExit(
        f"workspace default member must be the root package only, got {sorted(default_members)}"
    )
for vendored in ("crates/crossterm/Cargo.toml", "crates/ratatui-image/Cargo.toml"):
    if vendored in members:
        raise SystemExit(f"vendored fork must remain excluded from the workspace: {vendored}")
    with open(os.path.join(root, vendored), encoding="utf-8") as manifest:
        if "\n[workspace]\n" not in manifest.read():
            raise SystemExit(
                f"excluded vendored fork needs a standalone [workspace] boundary: {vendored}"
            )

core = packages["crates/yututui-core/Cargo.toml"]
if core["publish"] != []:
    raise SystemExit("yututui-core must remain private (publish = false)")

root_package = packages["Cargo.toml"]
if root_package["publish"] != []:
    raise SystemExit(
        "root package must remain private because it depends on private workspace crates"
    )
core_dependencies = [
    dependency
    for dependency in root_package["dependencies"]
    if dependency["name"] == "yututui-core"
]
expected_core_path = os.path.join(root, "crates", "yututui-core")
if len(core_dependencies) != 1 or os.path.realpath(core_dependencies[0]["path"] or "") != expected_core_path:
    raise SystemExit("root package must depend on the in-workspace yututui-core path")
if "yututui-core/ts-export" not in root_package["features"].get("ts-export", []):
    raise SystemExit("root ts-export feature must forward to yututui-core/ts-export")
' "$PWD"

echo "workspace layout ok"
