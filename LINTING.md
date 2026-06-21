# Vault service linting and safety gate

This stack is meant to be unforgiving: warnings are treated as errors and formatting must already be clean before code lands.

## Tools
- Ruff formatter + linter (`select = ["ALL"]`, preview rules on, opinionated import sorting)
- Mypy in `--strict` mode with the SQLAlchemy plugin
- Bandit security scanner
- Pip-audit against runtime and dev requirements (`--strict`)
- ESLint (React + hooks + a11y + import + security rules, warnings disabled) and Stylelint
- Prettier checks for JS/CSS/HTML assets

## One-time setup
```bash
cd vault-service
python -m venv .venv && source .venv/bin/activate
python -m pip install --upgrade pip
python -m pip install -r requirements.txt -r requirements-dev.txt
npm install
pre-commit install --config .pre-commit-config.yaml --hook-type pre-commit --hook-type pre-push
```

## Required gate before committing/pushing
```bash
# Run everything against the whole tree
pre-commit run --all-files --config vault-service/.pre-commit-config.yaml
```

Notes:
- `pip-audit` hits the network to fetch vulnerability data.
- If new dependencies are added, update both `requirements*.txt` and rerun `pip-audit`.
