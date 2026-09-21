#!/usr/bin/env python3
"""Validate capability-matrix system-test references without compiling.

The Rust matrix deliberately stores test names as strings so it can remain a
small product inventory. Rust cannot type-check a string against a test in a
different integration-test crate, so this validator closes that otherwise
silent drift: every ``system_test`` entry must resolve to a Rust function in
the workspace's ``crates`` tree.

This is an offline, dependency-free check intended for local runs and CI:

    scripts/e2e/validate_capability_matrix.py
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


SYSTEM_TEST_FIELD_RE = re.compile(r"\bsystem_test\s*:\s*")
SYSTEM_TEST_NAME_RE = re.compile(r'"([A-Za-z_][A-Za-z0-9_]*)"')
FUNCTION_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
MATRIX_TEST_INVOCATION_RE = re.compile(
    r"\b(?:matrix_test|current_thread_matrix_test)\s*!"
)
IDENTIFIER_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
OPEN_TO_CLOSE = {"{": "}", "[": "]", "(": ")"}


def _blank_non_code(source: str) -> str:
    """Replace comments and literals while preserving Rust token positions.

    This is intentionally a lexer-sized helper, not a Rust parser. The
    validator only needs to avoid treating names in comments and string/char
    literals as declarations. Newlines are retained so diagnostics still
    point at the original source locations.
    """

    chars = list(source)
    length = len(source)

    def blank(start: int, end: int) -> None:
        for index in range(start, min(end, length)):
            if chars[index] not in "\r\n":
                chars[index] = " "

    def raw_string_end(start: int) -> int | None:
        prefix_length = 0
        if source.startswith(("br", "rb"), start):
            prefix_length = 2
        elif source.startswith("r", start):
            prefix_length = 1
        else:
            return None

        cursor = start + prefix_length
        while cursor < length and source[cursor] == "#":
            cursor += 1
        if cursor >= length or source[cursor] != '"':
            return None
        hashes = source[start + prefix_length : cursor]
        terminator = '"' + hashes
        closing = source.find(terminator, cursor + 1)
        return length if closing < 0 else closing + len(terminator)

    def quoted_end(start: int, quote: str) -> int:
        cursor = start + 1
        while cursor < length:
            if source[cursor] == "\\":
                cursor += 2
                continue
            if source[cursor] == quote:
                return cursor + 1
            cursor += 1
        return length

    index = 0
    while index < length:
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = length if end < 0 else end
            blank(index, end)
            index = end
            continue

        if source.startswith("/*", index):
            end = index + 2
            depth = 1
            while end < length and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank(index, end)
            index = end
            continue

        raw_end = raw_string_end(index)
        if raw_end is not None:
            blank(index, raw_end)
            index = raw_end
            continue

        if source[index] == '"':
            end = quoted_end(index, '"')
            blank(index, end)
            index = end
            continue

        # Avoid treating a lifetime such as `'user` as a character literal.
        # Rust character literals are short enough that the closing quote is
        # immediately after one code point or one escape sequence.
        if source[index] == "'":
            cursor = index + 1
            if cursor < length and source[cursor] == "\\":
                cursor += 2
            else:
                cursor += 1
            if cursor < length and source[cursor] == "'":
                blank(index, cursor + 1)
                index = cursor + 1
                continue

        index += 1

    return "".join(chars)


def _skip_balanced(code: str, start: int) -> int | None:
    opener = code[start] if start < len(code) else ""
    if opener not in OPEN_TO_CLOSE:
        return None
    stack = [OPEN_TO_CLOSE[opener]]
    index = start + 1
    while index < len(code):
        if code[index] in OPEN_TO_CLOSE:
            stack.append(OPEN_TO_CLOSE[code[index]])
        elif stack and code[index] == stack[-1]:
            stack.pop()
            if not stack:
                return index + 1
        index += 1
    return None


def _matrix_test_names_from_code(code: str) -> set[str]:
    names: set[str] = set()
    for invocation in MATRIX_TEST_INVOCATION_RE.finditer(code):
        cursor = invocation.end()
        while cursor < len(code) and code[cursor].isspace():
            cursor += 1
        if cursor >= len(code) or code[cursor] not in OPEN_TO_CLOSE:
            continue
        body_start = cursor + 1
        while body_start < len(code):
            while body_start < len(code) and code[body_start].isspace():
                body_start += 1
            if code.startswith("#[", body_start):
                attribute_end = _skip_balanced(code, body_start + 1)
                if attribute_end is None:
                    break
                body_start = attribute_end
                continue
            match = IDENTIFIER_RE.match(code, body_start)
            if match:
                names.add(match.group(0))
            break
    return names


def collect_system_tests_from_source(source: str) -> list[str]:
    code = _blank_non_code(source)
    names: list[str] = []
    for field in SYSTEM_TEST_FIELD_RE.finditer(code):
        # `_blank_non_code` intentionally hides the target literal too. Use
        # the match's original span to find the colon, then read only the
        # literal immediately following that field; do not use `field.end()`
        # because the hidden literal is indistinguishable from whitespace in
        # the stripped source.
        colon = source.find(":", field.start(), field.end())
        cursor = colon + 1
        while cursor < len(source) and source[cursor].isspace():
            cursor += 1
        value = SYSTEM_TEST_NAME_RE.match(source, cursor)
        if value:
            names.append(value.group(1))
    return list(dict.fromkeys(names))


def collect_system_tests(matrix: Path) -> list[str]:
    return collect_system_tests_from_source(matrix.read_text(encoding="utf-8"))


def collect_rust_functions_from_source(source: str) -> set[str]:
    code = _blank_non_code(source)
    functions = set(FUNCTION_RE.findall(code))
    functions.update(_matrix_test_names_from_code(code))
    return functions


def collect_rust_functions(crate_root: Path) -> set[str]:
    functions: set[str] = set()
    for path in crate_root.rglob("*.rs"):
        if any(part in {"target", ".git"} for part in path.parts):
            continue
        source = path.read_text()
        functions.update(collect_rust_functions_from_source(source))
    return functions


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="repository root (defaults to the checkout containing this script)",
    )
    args = parser.parse_args()
    root = args.root.resolve()
    matrix = root / "crates/astra-harness/src/capability_matrix.rs"
    if not matrix.is_file():
        print(f"error: capability matrix not found: {matrix}", file=sys.stderr)
        return 2

    names = collect_system_tests(matrix)
    functions = collect_rust_functions(root / "crates")
    missing = [name for name in names if name not in functions]
    if missing:
        print("missing capability system-test functions:", file=sys.stderr)
        for name in missing:
            print(f"  {name}", file=sys.stderr)
        return 1

    print(f"capability matrix: {len(names)} system-test references resolved")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
