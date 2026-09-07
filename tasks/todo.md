# Tasks

GitHub Issue tracker is configured for this repository, but `gh` cannot read the local config in this environment (`C:\Users\xingr\AppData\Roaming\GitHub CLI\config.yml: Access is denied`). Until that is fixed, this checklist is the local source of truth for the current implementation.

- [x] Task 1: database interfaces for historical read-default and full-text updates
- [x] Task 2: Ruanyifeng archive parser, URL dedupe, 50-item cursor
- [x] Task 3: history backfill workflow with progress/pause/resume/retry
- [x] Task 4: on-demand full-text workflow and cache update
- [x] Task 5: GUI integration for backfill and full-text actions
- [x] Task 6: tests, build, packaging validation

## Checkpoints

- [x] Foundation parser and database tests pass
- [x] Background workflows compile and pass the library test suite
- [x] UI flow is exposed in the feed settings and article toolbar
- [x] Release build succeeds
