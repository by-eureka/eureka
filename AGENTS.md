# Repository workflow

- Keep `main` as the checked stable version. Do not merge, push, or promote changes
  to `main` without my explicit approval for that change.
- Use `dev` for integration of new work. Start feature branches from `dev`, track
  concrete work in issues, and target PRs at `dev` unless I explicitly request a
  stable promotion or fix. Approval to work on dev is not approval to merge main.
- Keep the stable and development checkouts, packages, runtime images, networks
  and services separate. Follow `CONTRIBUTING.md` and use the dev defaults in
  `scripts/env.sh`. Never replace dependencies used by active runs.
- Preserve uncommitted work. Use a separate worktree or clone when needed.
- Run checks relevant to the change and require green CI before integrating into
  `dev`. Do not bypass branch protection or enable auto-merge.
- Write issues, commit messages, PR descriptions and project updates in my voice.
  Do not add assistant attribution. Keep credentials out of commits and logs.
