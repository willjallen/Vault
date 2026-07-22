#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

failures=()

while IFS= read -r -d '' file; do
  matches="$(grep -nE '#\[cfg\(test\)\]|#\[test\]|(^|[[:space:]])mod[[:space:]]+tests[[:space:]]*\{' "$file" || true)"
  if [[ -n "$matches" ]]; then
    while IFS= read -r line; do
      failures+=("$file:$line")
    done <<< "$matches"
  fi
done < <(find vault/server -path '*/src/*' -type f -name '*.rs' -print0)

while IFS= read -r -d '' path; do
  failures+=("$path")
done < <(find vault/server -path '*/src/tests*' -print0)

if (( ${#failures[@]} == 0 )); then
  exit 0
fi

printf '%s\n' 'Rust tests must live under crate tests/ directories, not inline in source files.'
printf '%s\n' 'Move these tests into grouped files under vault/server/tests/:'
printf '  %s\n' "${failures[@]}"
exit 1
