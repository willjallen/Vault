# Agent Instructions

Before finishing any commit, run the full repository gate:

```bash
pre-commit run --all-files --config .pre-commit-config.yaml
```

Do not run this after each and every change you make. It takes a lot of time to run. Be judicious about running it, but it must be run before a commit is authored.

That command is the source of truth for formatting, linting, type checking, audits, and tests. If it fails, fix the failure and rerun the same command until it passes.
If the full gate fails only because an auto-fixing hook changed formatting, rerun only that hook—not the full repository gate.

Do not opportunistically change dependency versions while working on unrelated code. Dependency upgrades, including security or strongly recommended upgrades, must be made in a separate dependency-only commit.

When adding entries to CHANGELOG.txt:
- We do not require a 1:1 correspondence to commits. Similar commits can be grouped into one line.
- Prefer concise, user-facing entries. Every clause should add meaningful information about the release.
- Preserve technical specificity when it explains an important capability, constraint, compatibility concern, or security, reliability, performance, or operational impact. Concise does not mean vague.
- Omit incidental implementation details, internal mechanics, and development-only affordances unless users or operators need to know about them.
- Group categories together consistently between versions, and within categories order by relative "significance".
- One user-visible outcome gets at most one changelog entry, even when its implementation spans the client, server, telemetry, documentation, and tests. Do not decompose an outcome by layer, commit, required fix, or test case.
- Do not add separate `test:`, `refactor:`, or `docs:` entries when those changes only support an already-described `fix:` or `feat:` outcome.
- Keep each entry to one sentence and normally no more than 25 words. Exceed that only when a user or operator needs a concrete compatibility, security, data-safety, or operational constraint.
- Before considering the changelog finished, reread only the entries added or changed for the current work. If two entries can be accurately represented by one user-facing sentence, merge them.

When committing, view the recent commits to get an understanding of
the commit message format. Use only [labels] that appear in CHANGELOG.txt
