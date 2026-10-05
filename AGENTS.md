# AgentsMon contributor guide

AgentsMon is a Rust/GPUI desktop application. Keep provider-specific behavior outside the core and UI crates.

## Commands

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
claude plugin validate .
```

## Architecture rules

- `agentsmon-core` must remain pure Rust and provider/OS independent.
- New agents implement `AgentProvider` (including `accent_rgb` for the card ring colour); do not add provider conditionals to `MonitorView`.
- Session order is in-memory only. Custom card/group names persist in `custom-names.json` under the data directory, keyed by provider/session ID and opaque group key; retain them when sessions disappear or a provider returns an empty snapshot. Only names explicitly typed in AgentsMon are saved, never provider titles or snapshots. Hidden session IDs (× button) persist separately in `hidden-sessions.json` as IDs and timestamps only, so a restart does not bring closed sessions back; a hidden session still reappears on new activity.
- Never read or persist conversation content. Claude integration may consume hook metadata, metadata-only session registry status, status-line context metrics, transcript token counters (`usage` numbers only — never message text) and the session title lines Claude Code writes to the transcript (`ai-title` / `custom-title`). Codex integration may read the thread name from `~/.codex/session_index.jsonl`, never the `threads.title` column of Codex's SQLite state, which holds the first user message. OpenClaw integration asks the Gateway for `operator.read` only, advertises `session-scoped-events`, never subscribes, and decodes only the `sessions.list` fields listed in `docs/openclaw-gateway.md` (`label`/`displayName` for titles, never `derivedTitle`, `lastMessagePreview` or activity summaries). The shared Gateway token typed in the connect window is never persisted; only the minted device token is.
- Grouping is resolved in `AgentStore::group_key` (today: working directory; sessions in a linked git worktree use their main checkout, resolved from `.git` link files in `agentsmon/src/git_worktree.rs`) and the UI only sees opaque `GroupKey`s; manual grouping later means a new variant and an assignment map, not UI changes.
- Preserve an existing Claude `statusLine` exactly and restore it only if AgentsMon still owns the setting.
- UI state changes must be broadcast to every display panel.
- Keep the no-tray product decision. The normal Dock/taskbar application presence is intentional.

## Local source IPC

- `crates/local-source-ipc` is vendored identically in LeSwitcheur and defines the versioned contract, socket transport and golden v1 fixture; see its README. Neither repository requires a sibling checkout to build.
- `agentsmon/src/local_source.rs` publishes visible store metadata after every broadcast. Custom session/group names, states and unread notifications stay authoritative in AgentsMon. Never export conversation content or native focus targets.
- The macOS/Unix socket exists only while AgentsMon runs. Unsupported versions, malformed requests, expired commands and unavailable sessions fail safely. Windows currently exposes no local source transport.
- IPC focus, session-card focus and notification focus share `focus_agent_result`; for IPC, clear unread state only after successful focus. A user click on a card or notification clears it even when the host cannot be focused (e.g. Warp). Do not add a separate activation implementation for IPC consumers.

## Manual regression checks

- Before changing the Codex desktop observer, read `docs/codex-desktop-ipc.md`. Keep incoming IPC decoding restricted to routing/status metadata; never log raw frames or Serde error messages containing input values. Preserve bounded background I/O, hook/lifecycle fallback and the persisted hourly diagnostic-notification limit.
- Before changing the OpenClaw Gateway client, read `docs/openclaw-gateway.md`. Keep the read-only scope, the field allowlist and the park-on-credential-failure policy (no retry loop on refused auth).
- Before changing Claude interruption handling, read `docs/claude-session-status.md`. Preserve explicit lifecycle timestamps, PID/session validation and fallback for older Claude processes. Silence or low CPU usage never establishes that a session is idle.

- One panel per physical display, bottom-left and always on top.
- Screen hotplug and resolution changes reconcile panels without duplicates.
- Dragging on one panel changes all panels; clicking the × never focuses the session.
- Rename a session and a group, restart AgentsMon, and check both names. Close/reopen the project, then clear each name and restart again: the cleared names must not return. See `docs/custom-names.md` for storage and recovery.
- Context values 0, 1, 50, 99, 100 and unavailable render correctly.
- Claude states transition through idle, active, waiting and compacting.
- Plugin installation is automatic and idempotent; uninstall preserves settings edited after installation.
- macOS focus works across common terminal/IDE hosts; Windows falls back to the host window.
