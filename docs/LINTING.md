# Vault service linting and safety gate

This stack is meant to be unforgiving: compiler and linter warnings are treated as errors, and formatting must already be clean before code lands.

Direct dependency versions are intentionally pinned exactly. Do not loosen pins or opportunistically upgrade packages in feature/fix work.

## Tools
- ESLint (React + hooks + a11y + import + security rules, warnings disabled) and Stylelint
- `npm audit --audit-level=moderate`
- `cargo-audit` 0.22.2 against RustSec
- OSV Scanner 2.3.8 against the Cargo and npm lockfiles
- Prettier checks for JS/CSS/HTML assets
- Rust formatting, Clippy, test-layout validation, and the full Rust test suite

## One-time setup
```bash
npm --prefix vault/client install
pre-commit install --config .pre-commit-config.yaml --hook-type pre-commit --hook-type pre-push
```

`pre-commit` is expected to be installed as a system/user tool, the same way Rust and Node tooling are installed. This repository does not carry a Python virtualenv or Python dependency lock for the gate.

One-off Python utilities are allowed when they use the standard library and are not required by the repository gate.

The gate requires Linux/WSL or macOS. It downloads checksum-pinned `cargo-audit` and OSV Scanner releases on first use, then keeps their advisory/index data under `target/security-tools`; the combined repository-local security cache is roughly 150 MiB. It does not use global Cargo, Go, or pre-commit build caches.

## Required gate before committing/pushing
```bash
# Run everything against the whole tree
pre-commit run --all-files --config .pre-commit-config.yaml
```

Notes:
- `npm audit`, `cargo audit`, and OSV Scanner hit the network for current advisory data.
- The gate checks security advisories and installed dependency consistency; it intentionally does not run broad "latest available" upgrade checks.
- Time-bounded advisory exceptions and their reachability reasons live in `osv-scanner.toml`. `extras/security-audit.sh` mirrors the RustSec IDs for `cargo audit`; OSV Scanner enforces their expiry for both sources.
- `cargo audit` currently reports, but permits, the two known yanked `spin` versions. Yanked versions are not vulnerability advisories; replacing them requires a separately reviewed dependency-only commit.
- Security or strongly recommended dependency upgrades should be handled in a separate dependency-only commit.
- If new dependencies are added, update `Cargo.lock` or `package-lock.json` as appropriate and rerun the gate.
