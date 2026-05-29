# Contributing

Thanks for your interest in runic!

## Reporting bugs

Open an issue on the [GitHub tracker](https://github.com/quazardous/runic/issues)
with:

- What you tried to do.
- What you expected to happen.
- What actually happened (paste error messages / unexpected output; `RUNIC_LOG=runic=debug`
  gives more detail).
- Your environment (OS, `rustc --version`, and the relevant config — redact
  credentials).

A short reproducible case is worth pages of prose.

## Getting help

For usage questions (as opposed to bug reports), open an issue tagged
`question` on the tracker.

## Sending a pull request

1. Fork and branch off `main` (one feature per branch).
2. Keep the diff focused — small PRs review fast.
3. Add or update tests as needed; the suite must stay green:
   ```bash
   cargo test
   ```
4. Match the project's style — no warnings:
   ```bash
   cargo fmt
   cargo clippy --all-targets
   ```
5. Update `CHANGELOG.md` under `## [Unreleased]` for any user-facing change.
6. Open the PR. Describe the **what** and the **why**; mechanical diff details
   belong in the commit messages.

The Windows tray crate (`runic-tray/`) is a standalone crate excluded from the
root build (its GUI deps need Windows); see `docs/dev/windows-setup.md` to work
on it.

## Commit messages

Keep the subject line ≤ 72 chars, imperative mood, followed by a blank line and
a body that explains the *why*.

## Code of conduct

Be kind and assume good faith.
