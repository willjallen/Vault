# Agent Instructions

Before finishing any code change, run the full repository gate:

```bash
.venv/bin/pre-commit run --all-files --config .pre-commit-config.yaml
```

That command is the source of truth for formatting, linting, type checking, audits, and tests. If it fails, fix the failure and rerun the same command until it passes.

Do not opportunistically change dependency versions while working on unrelated code. Dependency upgrades, including security or strongly recommended upgrades, must be made in a separate dependency-only commit.
