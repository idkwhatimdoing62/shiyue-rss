# ADR-0013: Centralize Desktop Runtime and Settings

- Status: Accepted
- Date: 2026-08-26
- Decision owners: project maintainer and Codex implementation session

## Context

Desktop behavior was spread across the crate root, `config.rs`, `notify.rs`, and `gui.rs`. `GuiApp` owned the TOML path and mutable `Config`, wrote settings directly, created the native tray, interpreted global menu IDs, tracked independent `hidden`, `quitting`, and `focused` flags, intercepted close requests, selected notification policy, and called the operating system notification API. Startup separately resolved paths, initialized logging, installed fonts and style, and built eframe options.

That distribution made ordinary GUI changes capable of breaking process lifetime or settings durability. A tray creation failure aborted startup; close behavior could leave an invisible process; settings writes were not atomic; a failed write could still leave an in-memory value applied; and native behavior had no deterministic state-machine tests. CLI and GUI also loaded the same settings through ad-hoc calls instead of one version-aware seam.

## Decision

Introduce `Desktop Runtime & Settings` as the sole Module that owns desktop process integration and settings persistence.

- The crate-level Interface is `desktop_runtime::launch()` for the GUI and a narrow `command_environment()` projection for the CLI. The CLI receives paths and validated non-secret settings, but does not create a tray, window, notification host, or desktop session.
- GUI receives a `DesktopSession`. Its Interface exposes a read-only settings snapshot, supported UI scale values, `apply(SettingsChange, ctx)`, `poll(ctx) -> Vec<DesktopIntent>`, and notification intent. GUI never receives the settings file path or native tray/menu identifiers.
- Settings use an explicit `settings_version`. Unversioned legacy TOML is version zero and upgrades in memory to the current version without losing values. A future unsupported version, malformed TOML, invalid scale, or invalid refresh configuration fails explicitly and is never overwritten automatically.
- Settings changes follow validate → same-directory temporary file → flush → atomic replace → publish in-memory snapshot → immediate desktop effect. If persistence fails, the previous file, snapshot, and UI behavior remain active.
- API credentials are not fields in the settings schema. Windows Credential Manager remains the secret store; Knowledge Processing owns provider connection tests and task execution.
- Native host behavior is one explicit state machine: `Visible`, `Hidden`, or irreversible `Exiting`. When a tray exists, close hides the window; when tray creation fails, close exits normally so the process cannot become invisible. Tray refresh emits one semantic `RefreshAllFeeds` intent. Once exiting begins, later show, hide, refresh, and close events are ignored.
- Notification policy belongs to the Runtime. New-article notifications are suppressed while focused or when notifications are disabled. RSS Refresh Workflow only reports its completed run; it does not call the operating system.
- Fonts, theme, zoom, eframe window options, path resolution, and logging initialization are Runtime Implementation details.
- Stable Route persistence remains an eframe storage concern adopted by Desktop Interaction. Modal, Panel, Popover, Notice, and other transient UI state are not persisted.

## Module seam and ownership

`src/desktop_runtime/mod.rs` is the facade. `settings.rs` owns paths, version-aware loading, validation, and atomic TOML persistence. `host.rs` owns native tray adaptation and the window lifetime reducer. `style.rs` owns eframe options, embedded fonts, theme, and zoom initialization.

The Module has private external Seams:

- `SettingsStore`: production atomic TOML Adapter and deterministic memory Adapter in tests.
- native tray Adapter: converts operating-system menu events into typed host events and wakes egui.
- `NotificationHost`: production `notify-rust` Adapter and recording Adapter in tests.

RSS Refresh Workflow, Feed Subscription Lifecycle, Knowledge Processing, Local Data Maintenance, Article Library Lifecycle, Resource Library Lifecycle, and Desktop Route/Modal state remain separate Modules. Desktop Runtime may compose them at startup but does not interpret or persist their domain state.

## Verification contract

- Legacy unversioned settings load with preserved values and a current in-memory version.
- Invalid scale, corrupt TOML, and unsupported future versions return an explicit error without overwriting the source file.
- Atomic persistence round-trips and leaves no temporary file after success.
- A failed SettingsStore write cannot publish a new in-memory snapshot.
- Close hides with a tray and exits without a tray.
- `Exiting` ignores all later host events.
- A tray refresh produces exactly one refresh intent.
- Focused and notifications-disabled states never invoke the NotificationHost.
- Serialized settings contain no API key field or credential-shaped value.
- GUI source contains no direct settings-path persistence, native tray/menu handling, notification API call, or parallel hidden/quitting flags.
- Full tests, strict Clippy, and release build must pass.

## Implementation record

Implemented on 2026-08-26. `src/desktop_runtime/` now provides the facade and three private implementations. `run_gui` delegates to `launch`; CLI uses the command-environment projection. `GuiApp` owns only a `DesktopSession` and adopts semantic intents. Versioned settings validate before an atomic Windows `MoveFileExW` replacement, and the active snapshot changes only after persistence succeeds. Tray initialization degrades safely, native menu events wake egui, the host reducer owns visibility and exit state, and notification policy is executed behind a private Adapter. Runtime resources are declared last in `GuiApp`, so workflow engines and database participants drop before the tray host.

## Consequences

Desktop lifecycle rules and settings durability now have strong Locality. The Module has greater Depth: a compact Interface hides paths, logging, TOML compatibility, atomic replacement, style installation, tray events, viewport commands, focus policy, and notifications. GUI code can evolve without carrying native process state, and the CLI shares validated settings without accidentally starting desktop infrastructure.

The trade-off is that Desktop Runtime is platform-aware and depends directly on eframe and Windows atomic replacement behavior. It must remain a composition boundary rather than becoming an application god object. Domain workflows therefore stay outside the Module, and new settings are added only when they describe actual cross-session behavior.

## Rejected alternatives

- Keep tray and settings code in `gui.rs`: preserves the high-coupling hotspot and makes native lifecycle behavior hard to test.
- Make Desktop Runtime own RSS, AI, database maintenance, or library semantics: would create a broad application god module with poor Depth.
- Persist directly and mutate memory first: can lose the previous configuration or report a setting as active after a failed write.
- Always hide on close: produces an invisible, unrecoverable process when tray creation fails.
- Store the API key in TOML: leaks secrets into backups, diagnostics, and ordinary filesystem access.
- Add a public plugin abstraction for host or notification implementations: there is one production platform Adapter and one test substitute, so a public extension surface would be shallow.
