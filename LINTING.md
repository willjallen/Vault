# Vault service linting and safety gate

This stack is meant to be unforgiving: warnings are treated as errors and formatting must already be clean before code lands.

Direct dependency versions are intentionally pinned exactly. Do not loosen pins or opportunistically upgrade packages in feature/fix work.

## Tools
- ESLint (React + hooks + a11y + import + security rules, warnings disabled) and Stylelint
- `npm audit --audit-level=moderate`
- Prettier checks for JS/CSS/HTML assets
- Rust formatting, Clippy, test-layout validation, and the full Rust test suite

## One-time setup
```bash
npm install
pre-commit install --config .pre-commit-config.yaml --hook-type pre-commit --hook-type pre-push
```

`pre-commit` is expected to be installed as a system/user tool, the same way Rust and Node tooling are installed. This repository does not carry a Python virtualenv or Python dependency lock for the gate.

One-off Python utilities are allowed when they use the standard library and are not required by the repository gate.

## Required gate before committing/pushing
```bash
# Run everything against the whole tree
pre-commit run --all-files --config .pre-commit-config.yaml
```

Notes:
- `npm audit` also hits the network for advisory data.
- The gate checks security advisories and installed dependency consistency; it intentionally does not run broad "latest available" upgrade checks.
- Security or strongly recommended dependency upgrades should be handled in a separate dependency-only commit.
- If new dependencies are added, update `Cargo.lock` or `package-lock.json` as appropriate and rerun the gate.
