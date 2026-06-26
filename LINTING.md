# Vault service linting and safety gate

This stack is meant to be unforgiving: warnings are treated as errors and formatting must already be clean before code lands.

Direct dependency versions are intentionally pinned exactly. Do not loosen pins or opportunistically upgrade packages in feature/fix work.

## Tools
- Ruff formatter + linter with correctness, security, modernization, bugbear, and datetime rules
- Mypy in `--strict` mode with the SQLAlchemy plugin
- Bandit security scanner
- Pip-audit against runtime and dev requirements (`--strict`)
- `pip check` for installed Python dependency consistency
- ESLint (React + hooks + a11y + import + security rules, warnings disabled) and Stylelint
- `npm audit --audit-level=moderate`
- Prettier checks for JS/CSS/HTML assets
- Python unit tests

## One-time setup
```bash
python -m venv .venv && source .venv/bin/activate
python -m pip install --upgrade pip
python -m pip install -r requirements.txt -r requirements-dev.txt
npm install
pre-commit install --config .pre-commit-config.yaml --hook-type pre-commit --hook-type pre-push
```

## Required gate before committing/pushing
```bash
# Run everything against the whole tree
pre-commit run --all-files --config .pre-commit-config.yaml
```

Notes:
- `pip-audit` hits the network to fetch vulnerability data.
- `npm audit` also hits the network for advisory data.
- The gate checks security advisories and installed dependency consistency; it intentionally does not run broad "latest available" upgrade checks.
- Security or strongly recommended dependency upgrades should be handled in a separate dependency-only commit.
- If new dependencies are added, update both `requirements*.txt` or `package-lock.json` and rerun the gate.
