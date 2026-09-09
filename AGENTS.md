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
- New agents implement `AgentProvider`; do not add provider conditionals to `MonitorView`.
- Session order and hidden IDs are deliberately in-memory only.
- Never read or persist conversation content. Claude integration may consume hook metadata, status-line context metrics, and transcript token counters (`usage` numbers only — never message text).
- Preserve an existing Claude `statusLine` exactly and restore it only if AgentsMon still owns the setting.
- UI state changes must be broadcast to every display panel.
- Keep the no-tray product decision. The normal Dock/taskbar application presence is intentional.

## Manual regression checks

- One panel per physical display, bottom-left and always on top.
- Screen hotplug and resolution changes reconcile panels without duplicates.
- Dragging on one panel changes all panels; clicking the × never focuses the session.
- Context values 0, 1, 50, 99, 100 and unavailable render correctly.
- Claude states transition through idle, active, waiting and compacting.
- Plugin installation is automatic and idempotent; uninstall preserves settings edited after installation.
- macOS focus works across common terminal/IDE hosts; Windows falls back to the host window.
