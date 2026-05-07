#!/usr/bin/env python3
import argparse
import pathlib
import re
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
LOCKFILES = [
    ROOT / "examples/tauri-sqlx-vanilla/src-tauri/Cargo.lock",
]
INTERNAL_PACKAGE_MANIFESTS = [
    ROOT / "Cargo.toml",
    ROOT / "crates/assets/Cargo.toml",
    ROOT / "crates/aot/aarch64-apple-darwin/Cargo.toml",
    ROOT / "crates/aot/aarch64-unknown-linux-gnu/Cargo.toml",
    ROOT / "crates/aot/x86_64-pc-windows-msvc/Cargo.toml",
    ROOT / "crates/aot/x86_64-unknown-linux-gnu/Cargo.toml",
]
PACKAGE_START_RE = re.compile(r"^\s*\[\[package\]\]\s*$")
STRING_KEY_RE = re.compile(r'^\s*([A-Za-z0-9_-]+)\s*=\s*"([^"]*)"\s*(?:#.*)?$')
VERSION_LINE_RE = re.compile(r'^(\s*version\s*=\s*)"[^"]*"(\s*(?:#.*)?)$')


def load_internal_versions() -> dict[str, str]:
    versions = {}
    for manifest in INTERNAL_PACKAGE_MANIFESTS:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        package = data.get("package")
        if not isinstance(package, dict):
            raise SystemExit(f"{manifest.relative_to(ROOT)} is missing [package]")
        name = package.get("name")
        version = package.get("version")
        if not isinstance(name, str) or not isinstance(version, str):
            raise SystemExit(f"{manifest.relative_to(ROOT)} is missing package.name/version")
        versions[name] = version
    return versions


def strip_newline(line: str) -> tuple[str, str]:
    if line.endswith("\r\n"):
        return line[:-2], "\r\n"
    if line.endswith("\n"):
        return line[:-1], "\n"
    return line, ""


def string_key(line: str, key: str) -> str | None:
    body, _ = strip_newline(line)
    match = STRING_KEY_RE.match(body)
    if match and match.group(1) == key:
        return match.group(2)
    return None


def replace_version_line(line: str, version: str) -> str:
    body, newline = strip_newline(line)
    match = VERSION_LINE_RE.match(body)
    if not match:
        raise SystemExit(f"cannot update Cargo.lock version line: {line.rstrip()}")
    return f'{match.group(1)}"{version}"{match.group(2)}{newline}'


def package_block_ranges(lines: list[str]) -> list[tuple[int, int]]:
    starts = [idx for idx, line in enumerate(lines) if PACKAGE_START_RE.match(line)]
    return [
        (start, starts[pos + 1] if pos + 1 < len(starts) else len(lines))
        for pos, start in enumerate(starts)
    ]


def check_lockfile_contains_path_packages(lockfile: pathlib.Path, versions: dict[str, str]) -> None:
    data = tomllib.loads(lockfile.read_text(encoding="utf-8"))
    packages = data.get("package")
    if not isinstance(packages, list):
        raise SystemExit(f"{lockfile.relative_to(ROOT)} is missing [[package]] entries")

    present = {
        package.get("name")
        for package in packages
        if isinstance(package, dict) and package.get("name") in versions and "source" not in package
    }
    missing = sorted(set(versions) - present)
    if missing:
        raise SystemExit(
            f"{lockfile.relative_to(ROOT)} is missing internal path packages: {', '.join(missing)}"
        )


def sync_lockfile(lockfile: pathlib.Path, versions: dict[str, str]) -> list[str]:
    check_lockfile_contains_path_packages(lockfile, versions)
    lines = lockfile.read_text(encoding="utf-8").splitlines(keepends=True)
    changes = []

    for start, end in package_block_ranges(lines):
        block = lines[start:end]
        name = None
        version_idx = None
        current_version = None
        has_source = False

        for offset, line in enumerate(block):
            if string_key(line, "source") is not None:
                has_source = True
            key_name = string_key(line, "name")
            if key_name is not None:
                name = key_name
            key_version = string_key(line, "version")
            if key_version is not None:
                version_idx = start + offset
                current_version = key_version

        if name not in versions or has_source:
            continue
        if version_idx is None or current_version is None:
            raise SystemExit(f"{lockfile.relative_to(ROOT)} package {name} is missing version")

        expected_version = versions[name]
        if current_version != expected_version:
            lines[version_idx] = replace_version_line(lines[version_idx], expected_version)
            changes.append(
                f"{lockfile.relative_to(ROOT)}: {name} {current_version} -> {expected_version}"
            )

    if changes:
        lockfile.write_text("".join(lines), encoding="utf-8")
    return changes


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="fail instead of writing updates")
    args = parser.parse_args()

    versions = load_internal_versions()
    all_changes = []
    for lockfile in LOCKFILES:
        before = lockfile.read_text(encoding="utf-8")
        changes = sync_lockfile(lockfile, versions)
        if args.check and changes:
            lockfile.write_text(before, encoding="utf-8")
        all_changes.extend(changes)

    if not all_changes:
        print("example lockfiles match internal package versions")
        return 0

    for change in all_changes:
        print(change, file=sys.stderr)
    if args.check:
        print("example lockfiles are stale; run `scripts/sync-example-lockfiles.py`", file=sys.stderr)
        return 1

    print("updated example lockfiles")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
