#!/usr/bin/env python3
"""Reject inline Rust tests outside crate-level tests directories."""

from __future__ import annotations

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
CRATES_DIR = ROOT / "crates"
PATTERNS = (
    re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]"),
    re.compile(r"#\s*\[\s*test\s*\]"),
    re.compile(r"#\s*\[\s*tokio::test\b"),
    re.compile(r"\bmod\s+tests\b"),
)


def is_allowed_test_path(path: Path) -> bool:
    parts = path.relative_to(ROOT).parts
    return len(parts) >= 4 and parts[0] == "crates" and parts[2] == "tests"


def main() -> int:
    violations: list[str] = []
    for path in sorted(CRATES_DIR.rglob("*.rs")):
        if is_allowed_test_path(path):
            continue
        for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if any(pattern.search(line) for pattern in PATTERNS):
                relative = path.relative_to(ROOT)
                violations.append(f"{relative}:{line_number}: {line.strip()}")

    if not violations:
        return 0

    print("Rust tests must live under crate tests/ directories, not inline in source files.", file=sys.stderr)
    print("Move these tests into grouped files under crates/<crate>/tests/:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
