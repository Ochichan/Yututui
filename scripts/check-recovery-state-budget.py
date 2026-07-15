#!/usr/bin/env python3
"""Ratchet the syntactic variant surface of recovery control enums."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Callable, Iterable


IDENTIFIER = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*\Z")
SCOPED_CONTROL_SUFFIXES = (
    "State",
    "Phase",
    "Lifecycle",
    "Publication",
    "Admission",
    "Completion",
    "Intent",
    "Origin",
    "Plan",
    "Kind",
    "Decision",
    "Stage",
    "Purpose",
    "Mode",
    "Operation",
    "Receipt",
    "Outcome",
    "Failure",
    "Error",
    "Disposition",
)


class CheckError(Exception):
    """A deterministic checker or manifest error."""


class RustParseError(CheckError):
    """The lightweight Rust lexer could not safely inspect a source file."""


@dataclass(frozen=True)
class BudgetEntry:
    group: str
    expected: int
    path: str
    enum_name: str
    line: int


@dataclass(frozen=True)
class AuditResult:
    core: int
    transition: int
    support: int
    enum_count: int
    errors: tuple[str, ...]

    def summary(self) -> str:
        return (
            "recovery-state budget ok: "
            f"core={self.core} transition={self.transition} "
            f"control={self.core + self.transition} support={self.support} "
            f"tracked={self.core + self.transition + self.support} "
            f"enums={self.enum_count}"
        )


def _raw_string_end(source: str, start: int) -> int | None:
    """Return the end of a Rust raw string beginning at start, if present."""

    cursor = start
    if source.startswith(("br", "rb", "cr", "rc"), cursor):
        cursor += 2
    elif source.startswith("r", cursor):
        cursor += 1
    else:
        return None
    hashes = 0
    while cursor < len(source) and source[cursor] == "#":
        hashes += 1
        cursor += 1
    if cursor >= len(source) or source[cursor] != '"':
        return None
    terminator = '"' + ("#" * hashes)
    end = source.find(terminator, cursor + 1)
    if end < 0:
        raise RustParseError("unterminated raw string")
    return end + len(terminator)


def _quoted_end(source: str, quote: int, delimiter: str) -> int:
    cursor = quote + 1
    while cursor < len(source):
        char = source[cursor]
        if char == "\\":
            cursor += 2
            continue
        if char == delimiter:
            return cursor + 1
        cursor += 1
    raise RustParseError(f"unterminated {delimiter} literal")


def _char_literal_end(source: str, quote: int) -> int | None:
    """Return a valid one-codepoint Rust character literal end, not a lifetime."""

    cursor = quote + 1
    if cursor >= len(source) or source[cursor] in {"'", "\n"}:
        return None
    if source[cursor] != "\\":
        cursor += 1
    elif source.startswith("\\u{", cursor):
        closing = source.find("}", cursor + 3)
        if closing < 0:
            raise RustParseError("unterminated unicode character escape")
        cursor = closing + 1
    elif source.startswith("\\x", cursor):
        cursor += 4
    else:
        cursor += 2
    if cursor < len(source) and source[cursor] == "'":
        return cursor + 1
    return None


def rust_tokens(source: str) -> list[str]:
    """Tokenize enough Rust syntax to locate enum bodies without parsing payload types."""

    tokens: list[str] = []
    cursor = 0
    length = len(source)
    while cursor < length:
        if source.startswith("//", cursor):
            newline = source.find("\n", cursor + 2)
            cursor = length if newline < 0 else newline + 1
            continue
        if source.startswith("/*", cursor):
            depth = 1
            cursor += 2
            while cursor < length and depth:
                if source.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif source.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            if depth:
                raise RustParseError("unterminated block comment")
            continue

        raw_end = _raw_string_end(source, cursor)
        if raw_end is not None:
            cursor = raw_end
            continue

        prefix_length = 0
        if source.startswith(("b\"", "c\""), cursor):
            prefix_length = 1
        if source[cursor + prefix_length : cursor + prefix_length + 1] == '"':
            cursor = _quoted_end(source, cursor + prefix_length, '"')
            continue

        char_prefix = 1 if source.startswith("b'", cursor) else 0
        if source[cursor + char_prefix : cursor + char_prefix + 1] == "'":
            quote = cursor + char_prefix
            char_end = _char_literal_end(source, quote)
            if char_end is not None:
                cursor = char_end
                continue

        char = source[cursor]
        if char.isspace():
            cursor += 1
            continue
        raw_identifier = re.match(r"r#[A-Za-z_][A-Za-z0-9_]*", source[cursor:])
        if raw_identifier:
            token = raw_identifier.group(0)
            tokens.append(token)
            cursor += len(token)
            continue
        identifier = re.match(r"[A-Za-z_][A-Za-z0-9_]*", source[cursor:])
        if identifier:
            token = identifier.group(0)
            tokens.append(token)
            cursor += len(token)
            continue
        tokens.append(char)
        cursor += 1
    return tokens


def _enum_body_start(tokens: list[str], name_index: int) -> int:
    angle = paren = square = 0
    for index in range(name_index + 1, len(tokens)):
        token = tokens[index]
        if token == "<":
            angle += 1
        elif token == ">" and angle:
            angle -= 1
        elif token == "(":
            paren += 1
        elif token == ")" and paren:
            paren -= 1
        elif token == "[":
            square += 1
        elif token == "]" and square:
            square -= 1
        elif token == "{" and not (angle or paren or square):
            return index
        elif token == ";" and not (angle or paren or square):
            break
    raise RustParseError(f"enum {tokens[name_index]} has no body")


def _matching_brace(tokens: list[str], opening: int) -> int:
    depth = 0
    for index in range(opening, len(tokens)):
        if tokens[index] == "{":
            depth += 1
        elif tokens[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise RustParseError("unterminated enum body")


def _strip_outer_attributes(segment: list[str]) -> list[str]:
    cursor = 0
    while cursor < len(segment) and segment[cursor] == "#":
        cursor += 1
        if cursor < len(segment) and segment[cursor] == "!":
            cursor += 1
        if cursor >= len(segment) or segment[cursor] != "[":
            raise RustParseError("malformed enum variant attribute")
        depth = 1
        cursor += 1
        while cursor < len(segment) and depth:
            if segment[cursor] == "[":
                depth += 1
            elif segment[cursor] == "]":
                depth -= 1
            cursor += 1
        if depth:
            raise RustParseError("unterminated enum variant attribute")
    return segment[cursor:]


def _variant_count(body: list[str], enum_name: str) -> int:
    segments: list[list[str]] = []
    segment: list[str] = []
    curly = paren = square = 0
    for token in body:
        if token == "=" and not (curly or paren or square):
            raise RustParseError(
                f"enum {enum_name} uses an explicit discriminant; "
                "recovery-state enums must use data-only variants"
            )
        if token == "," and not (curly or paren or square):
            segments.append(segment)
            segment = []
            continue
        segment.append(token)
        if token == "{":
            curly += 1
        elif token == "}" and curly:
            curly -= 1
        elif token == "(":
            paren += 1
        elif token == ")" and paren:
            paren -= 1
        elif token == "[":
            square += 1
        elif token == "]" and square:
            square -= 1
    if segment:
        segments.append(segment)

    count = 0
    for raw_segment in segments:
        candidate = _strip_outer_attributes(raw_segment)
        if not candidate:
            continue
        if not IDENTIFIER.fullmatch(candidate[0]):
            raise RustParseError(
                f"enum {enum_name} has an unrecognized variant near {candidate[0]!r}"
            )
        count += 1
    return count


def parse_enums(source: str) -> list[tuple[str, int]]:
    tokens = rust_tokens(source)
    enums: list[tuple[str, int]] = []
    cursor = 0
    while cursor < len(tokens):
        if tokens[cursor] != "enum":
            cursor += 1
            continue
        name_index = cursor + 1
        if name_index >= len(tokens) or not IDENTIFIER.fullmatch(tokens[name_index]):
            cursor += 1
            continue
        opening = _enum_body_start(tokens, name_index)
        closing = _matching_brace(tokens, opening)
        name = tokens[name_index].removeprefix("r#")
        enums.append((name, _variant_count(tokens[opening + 1 : closing], name)))
        cursor = closing + 1
    return enums


def discover_enum_names(source: str) -> list[str]:
    """Discover enum declarations without needing to count every unrelated enum body."""

    tokens = rust_tokens(source)
    names: list[str] = []
    cursor = 0
    while cursor + 1 < len(tokens):
        if tokens[cursor] != "enum" or not IDENTIFIER.fullmatch(tokens[cursor + 1]):
            cursor += 1
            continue
        name_index = cursor + 1
        opening = _enum_body_start(tokens, name_index)
        closing = _matching_brace(tokens, opening)
        names.append(tokens[name_index].removeprefix("r#"))
        cursor = closing + 1
    return names


def is_globally_budgeted_name(enum_name: str) -> bool:
    """Naming contract for recovery enums, including types introduced in new files."""

    return "Recover" in enum_name or enum_name.startswith(
        ("Resume", "Reconnect", "Restore")
    )


def is_recovery_path(path: str) -> bool:
    """Treat state-like enums in recovery/resume modules as budget candidates by location."""

    return any(
        "recovery" in part.lower() or "resume" in part.lower()
        for part in PurePosixPath(path).parts
    )


def parse_manifest(text: str) -> list[BudgetEntry]:
    entries: list[BudgetEntry] = []
    seen: set[tuple[str, str]] = set()
    for line_number, raw_line in enumerate(text.splitlines(), 1):
        if not raw_line or raw_line.startswith("#"):
            continue
        fields = raw_line.split("\t")
        if len(fields) != 4:
            raise CheckError(
                f"recovery-state manifest line {line_number}: expected 4 tab-separated fields"
            )
        if any(field != field.strip() or not field for field in fields):
            raise CheckError(
                f"recovery-state manifest line {line_number}: fields must be non-empty and unpadded"
            )
        group, expected_text, path, enum_name = fields
        if group not in {"core", "transition", "support"}:
            raise CheckError(
                f"recovery-state manifest line {line_number}: unknown group {group!r}"
            )
        if not expected_text.isdecimal() or int(expected_text) <= 0:
            raise CheckError(
                f"recovery-state manifest line {line_number}: variant count must be positive"
            )
        pure_path = PurePosixPath(path)
        if pure_path.is_absolute() or any(part in {"", ".", ".."} for part in pure_path.parts):
            raise CheckError(
                f"recovery-state manifest line {line_number}: path must be repository-relative"
            )
        if not path.startswith("src/") or pure_path.suffix != ".rs":
            raise CheckError(
                f"recovery-state manifest line {line_number}: source must be a src/*.rs path"
            )
        if not IDENTIFIER.fullmatch(enum_name):
            raise CheckError(
                f"recovery-state manifest line {line_number}: invalid enum name {enum_name!r}"
            )
        key = (path, enum_name)
        if key in seen:
            raise CheckError(
                f"recovery-state manifest line {line_number}: duplicate enum {path}:{enum_name}"
            )
        seen.add(key)
        entries.append(
            BudgetEntry(group, int(expected_text), path, enum_name, line_number)
        )
    if not entries:
        raise CheckError("recovery-state manifest has no budget entries")
    return entries


def audit(
    entries: Iterable[BudgetEntry],
    source_loader: Callable[[str], str],
    discovery_paths: Iterable[str] | None = None,
) -> AuditResult:
    ordered_entries = list(entries)
    parsed: dict[str, list[tuple[str, int]]] = {}
    failed_paths: set[str] = set()
    errors: list[str] = []
    core = transition = support = 0

    for path in dict.fromkeys(entry.path for entry in ordered_entries):
        try:
            parsed[path] = parse_enums(source_loader(path))
        except FileNotFoundError:
            errors.append(f"recovery-state source missing: {path}")
            failed_paths.add(path)
        except (OSError, UnicodeError) as error:
            errors.append(f"recovery-state source unreadable: {path}: {error}")
            failed_paths.add(path)
        except RustParseError as error:
            errors.append(f"recovery-state parse failed: {path}: {error}")
            failed_paths.add(path)

    budgeted = {(entry.path, entry.enum_name) for entry in ordered_entries}
    scoped_paths = {entry.path for entry in ordered_entries}
    for entry in ordered_entries:
        declarations = [
            count
            for name, count in parsed.get(entry.path, [])
            if name == entry.enum_name
        ]
        if not declarations:
            if entry.path in parsed:
                errors.append(
                    f"recovery-state enum missing: {entry.path}:{entry.enum_name}"
                )
            continue
        if len(declarations) != 1:
            errors.append(
                f"recovery-state enum ambiguous: {entry.path}:{entry.enum_name} "
                f"has {len(declarations)} declarations"
            )
            continue
        actual = declarations[0]
        if actual != entry.expected:
            errors.append(
                f"recovery-state budget mismatch: {entry.path}:{entry.enum_name} "
                f"expected {entry.expected} variants, found {actual}"
            )
        if entry.group == "core":
            core += actual
        elif entry.group == "transition":
            transition += actual
        else:
            support += actual

    discovered: list[tuple[str, str]] = []
    paths = (
        list(dict.fromkeys(discovery_paths))
        if discovery_paths is not None
        else list(dict.fromkeys(entry.path for entry in ordered_entries))
    )
    for path in sorted(paths):
        if path in failed_paths:
            continue
        try:
            names = (
                [name for name, _ in parsed[path]]
                if path in parsed
                else discover_enum_names(source_loader(path))
            )
        except FileNotFoundError:
            errors.append(f"recovery-state discovery source missing: {path}")
            continue
        except (OSError, UnicodeError) as error:
            errors.append(f"recovery-state discovery source unreadable: {path}: {error}")
            continue
        except RustParseError as error:
            errors.append(f"recovery-state discovery parse failed: {path}: {error}")
            continue
        discovered.extend(
            (path, enum_name)
            for enum_name in names
            if is_globally_budgeted_name(enum_name)
            or (
                (path in scoped_paths or is_recovery_path(path))
                and enum_name.endswith(SCOPED_CONTROL_SUFFIXES)
            )
        )
    for path, enum_name in sorted(discovered):
        if (path, enum_name) not in budgeted:
            errors.append(f"unbudgeted recovery-state enum: {path}:{enum_name}")

    return AuditResult(core, transition, support, len(ordered_entries), tuple(errors))


def _expect(label: str, condition: bool) -> None:
    if not condition:
        raise CheckError(f"recovery-state checker self-test failed: {label}")


def self_test() -> None:
    shaped = r'''
        pub enum Shaped<T>
        where
            T: Trait,
        {
            #[default]
            Unit,
            Tuple(Result<T, Error>),
            Struct { value: T, pair: (u8, u8) },
            Borrowed(&'static str),
            Lifetimes(&'a str, &'b str),
            Last,
        }
        const DECOY: &str = "enum HiddenState { Fake, }";
        /* enum CommentState { Fake, /* nested */ AlsoFake, } */
    '''
    _expect("variant shapes", parse_enums(shaped) == [("Shaped", 6)])
    payload_operators = r'''
        enum ResumeStage {
            A,
            B([u8; 1 << 2]),
            C(Type<{ 1 < 2 }>),
            D,
        }
    '''
    _expect(
        "const payload operators do not hide trailing variants",
        parse_enums(payload_operators) == [("ResumeStage", 4)],
    )
    for expression in ("1 << 2", "1 < 2"):
        try:
            parse_enums(f"enum ResumeStage {{ A = {expression}, B, C }}")
        except RustParseError as error:
            _expect("discriminant diagnostic", "explicit discriminant" in str(error))
        else:
            _expect("discriminant rejection", False)

    manifest = (
        "core\t2\tsrc/lifecycle.rs\tDemoRecoveryState\n"
        "transition\t2\tsrc/transition.rs\tDemoPlan\n"
        "support\t2\tsrc/support.rs\tDemoRecoveryOutcome\n"
    )
    entries = parse_manifest(manifest)
    sources = {
        "src/lifecycle.rs": "enum DemoRecoveryState { Open, Closed }",
        "src/transition.rs": "enum DemoPlan<T> { Idle, Run(T) }",
        "src/support.rs": "enum DemoRecoveryOutcome { Accepted, Rejected }",
    }
    loader = sources.__getitem__
    result = audit(entries, loader, sources)
    _expect("exact budget", not result.errors)
    _expect(
        "deterministic summary",
        result.summary()
        == (
            "recovery-state budget ok: core=2 transition=2 control=4 "
            "support=2 tracked=6 enums=3"
        ),
    )

    larger = dict(sources)
    larger["src/lifecycle.rs"] = (
        "enum DemoRecoveryState { Open, Closed, Restarting }"
    )
    _expect(
        "growth mismatch",
        audit(entries, larger.__getitem__, larger).errors
        == (
            "recovery-state budget mismatch: src/lifecycle.rs:DemoRecoveryState "
            "expected 2 variants, found 3",
        ),
    )
    smaller = dict(sources)
    smaller["src/lifecycle.rs"] = "enum DemoRecoveryState { Open }"
    _expect(
        "reduction mismatch",
        audit(entries, smaller.__getitem__, smaller).errors
        == (
            "recovery-state budget mismatch: src/lifecycle.rs:DemoRecoveryState "
            "expected 2 variants, found 1",
        ),
    )
    unlisted = dict(sources)
    unlisted["src/new_resume.rs"] = "enum ResumeStage { Waiting, Ready }"
    _expect(
        "unbudgeted state in a new file",
        audit(entries, unlisted.__getitem__, unlisted).errors
        == ("unbudgeted recovery-state enum: src/new_resume.rs:ResumeStage",),
    )
    reconnect = dict(sources)
    reconnect["src/transport.rs"] = "enum ReconnectPhase { Waiting, Ready }"
    _expect(
        "unbudgeted reconnect state in a new file",
        audit(entries, reconnect.__getitem__, reconnect).errors
        == ("unbudgeted recovery-state enum: src/transport.rs:ReconnectPhase",),
    )
    path_scoped = dict(sources)
    path_scoped["src/recovery/flow.rs"] = "enum FlowState { Waiting, Ready }"
    _expect(
        "unbudgeted state in a new recovery path",
        audit(entries, path_scoped.__getitem__, path_scoped).errors
        == ("unbudgeted recovery-state enum: src/recovery/flow.rs:FlowState",),
    )
    scoped = dict(sources)
    scoped["src/transition.rs"] += " enum LocalDisposition { Keep, Drop }"
    _expect(
        "unbudgeted projection in a tracked file",
        audit(entries, scoped.__getitem__, scoped).errors
        == (
            "unbudgeted recovery-state enum: "
            "src/transition.rs:LocalDisposition",
        ),
    )
    missing_enum = dict(sources)
    missing_enum["src/lifecycle.rs"] = "enum Other { Open, Closed }"
    _expect(
        "missing enum",
        audit(entries, missing_enum.__getitem__, missing_enum).errors
        == ("recovery-state enum missing: src/lifecycle.rs:DemoRecoveryState",),
    )
    missing_source = dict(sources)
    del missing_source["src/lifecycle.rs"]

    def load_missing(path: str) -> str:
        try:
            return missing_source[path]
        except KeyError as error:
            raise FileNotFoundError(path) from error

    _expect(
        "missing source",
        audit(entries, load_missing, missing_source).errors
        == ("recovery-state source missing: src/lifecycle.rs",),
    )
    try:
        parse_manifest(manifest + "core\t2\tsrc/lifecycle.rs\tDemoRecoveryState\n")
    except CheckError as error:
        _expect("duplicate diagnostic", "duplicate enum" in str(error))
    else:
        _expect("duplicate rejection", False)
    try:
        parse_manifest("core 2 src/x.rs DemoRecoveryState\n")
    except CheckError as error:
        _expect("malformed diagnostic", "tab-separated" in str(error))
    else:
        _expect("malformed rejection", False)
    print("recovery-state checker self-test ok")


def _read_source(root: Path, path: str) -> str:
    return (root / PurePosixPath(path)).read_text(encoding="utf-8")


def _rust_source_paths(root: Path) -> list[str]:
    source_root = root / "src"
    return sorted(path.relative_to(root).as_posix() for path in source_root.rglob("*.rs"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument(
        "--manifest",
        type=PurePosixPath,
        default=PurePosixPath("scripts/recovery-state-budget.tsv"),
    )
    args = parser.parse_args(argv)
    try:
        if args.self_test:
            self_test()
            return 0
        root = args.root.resolve()
        manifest_path = root / args.manifest
        entries = parse_manifest(manifest_path.read_text(encoding="utf-8"))
        result = audit(
            entries,
            lambda path: _read_source(root, path),
            _rust_source_paths(root),
        )
        if result.errors:
            for error in result.errors:
                print(f"error: {error}", file=sys.stderr)
            return 1
        print(result.summary())
        return 0
    except (CheckError, OSError, UnicodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
