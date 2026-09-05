# Issue tracker: GitHub

Issues and specs for this repo live as GitHub Issues in `idkwhatimdoing62/shiyue-rss`. Use the `gh` CLI for all operations.

## Conventions

- Create an issue with `gh issue create --title "..." --body "..."`.
- Read an issue with `gh issue view <number> --comments`.
- List issues with `gh issue list` and include labels when triaging.
- Apply or remove labels with `gh issue edit <number> --add-label "..."` or `--remove-label "..."`.

## Pull requests as a triage surface

PRs as a request surface: no.

## Blocking

Use GitHub issue dependencies when available. If native dependencies are unavailable, include a `Blocked by:` line in the issue body. Tickets are published in dependency order so later issues can reference earlier numbers.
