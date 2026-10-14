# Repository instructions

## GitHub access

- Never use the GitHub website or browser UI for GitHub work. Always use the GitHub plugin for repository information, pull requests, workflow status, and logs.
- If a needed plugin capability is unavailable, report the limitation; do not fall back to the website or GitHub CLI/HTTP API.
- Local Git commands remain allowed for commits, fetches, pushes, branches, and release tags.

## Commit messages

- Use Conventional Commits for every new commit: `type(scope): summary`. Scope is optional; use `!` only for an actual breaking change.
- Use a lowercase type from `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, or `revert`, and an imperative English summary.
- Examples: `feat(firmware): add full-image recovery`, `fix(transport): reject expired approval`, `docs: update launch instructions`.
- Preserve original commit IDs in existing hardware/build evidence. The original history is retained under `refs/archive/conventional-commits-20261012/`; rewriting messages does not recompile or revalidate those historical builds.
