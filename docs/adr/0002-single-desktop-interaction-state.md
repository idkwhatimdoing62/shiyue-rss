# Keep one explicit desktop interaction state

The desktop uses one interaction state as the source of truth for the active Route, Modal, route-owned Panel, Popover, Notice, and any guarded pending transition. Modal payloads live in the Modal state, article collections live in the Route, and a pure reducer is the only runtime lifecycle entry point; shadow payload fields, inferred modal priority, direct lifecycle mutation, and direct I/O inside transitions are rejected because they previously allowed stale dialogs to reopen, incompatible Route/Modal combinations to leave an orphaned blocker, and late asynchronous results to overwrite newer interaction state.

## Consequences

Only one Modal can exist and it never queues implicitly. `gui_modal` is the single project-specific presentation seam: it exhaustively owns the blocker, Foreground window layer, close button and Escape semantics, dirty-discard guard, focus policy, and per-kind presentation metadata, while feature adapters retain form rendering, validation, and domain writes. Dirty Modal and Panel drafts guard navigation through one explicit pending transition, completed or discarded async Modals invalidate their request identity so late results are ignored, Modal rendering is dispatched once at the GUI root, egui geometry remains outside the reducer, and only stable Routes are restored after restart.
