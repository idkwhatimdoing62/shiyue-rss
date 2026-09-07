# Tasks

GitHub Issue tracker is configured for this repository, but `gh` cannot read the local config in this environment (`C:\Users\xingr\AppData\Roaming\GitHub CLI\config.yml: Access is denied`). Until that is fixed, this checklist is the local source of truth for the current implementation.

- [ ] Task 1: database interfaces for historical read-default and full-text updates
- [ ] Task 2: Ruanyifeng archive parser, URL dedupe, 50-item cursor
- [ ] Task 3: history backfill workflow with progress/pause/resume/retry
- [ ] Task 4: on-demand full-text workflow and cache update
- [ ] Task 5: GUI integration for backfill and full-text actions
- [ ] Task 6: tests, build, packaging validation

## Checkpoints

- [ ] Foundation parser and database tests pass
- [ ] Background workflows pass focused tests
- [ ] UI flow is manually verifiable
- [ ] Release build succeeds
