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
- The application and the helper resolve their data and configuration directories through `agentsmon_providers::app_dirs` only (bundle identifier `fr.gmbl.AgentsMon`).
- UI strings are English only; there is no locale handling.
- Session order is in-memory only. Custom card/group names persist in `custom-names.json` under the data directory, keyed by provider/session ID and opaque group key; retain them when sessions disappear or a provider returns an empty snapshot. Only names explicitly typed in AgentsMon are saved, never provider titles or snapshots. Hidden session IDs (× button) persist separately in `hidden-sessions.json` as IDs and timestamps only, so a restart does not bring closed sessions back; a hidden session still reappears on new activity. Providers toggled off by clicking their bar icon persist as IDs in `hidden-providers.json`; they hide current and future sessions of that provider (no reappearance on activity, no completion notification) until toggled back on.
- Never read or persist conversation content. Claude integration may consume hook metadata, metadata-only session registry status, status-line context metrics, transcript token counters (`usage` numbers only — never message text) and the session title lines Claude Code writes to the transcript (`ai-title` / `custom-title`). Codex integration may read the thread name from `~/.codex/session_index.jsonl`, never the `threads.title` column of Codex's SQLite state, which holds the first user message. OpenClaw integration asks the Gateway for `operator.read` only, advertises `session-scoped-events`, never subscribes, and decodes only the `sessions.list` fields listed in `docs/openclaw-gateway.md` (`label`/`displayName` for titles, never `derivedTitle`, `lastMessagePreview` or activity summaries). The shared Gateway token typed in the connect window is never persisted; only the minted device token is.
- Grouping is resolved in `AgentStore::group_key` (today: working directory; sessions in a linked git worktree use their main checkout, resolved from `.git` link files in `agentsmon/src/git_worktree.rs`) and the UI only sees opaque `GroupKey`s; manual grouping later means a new variant and an assignment map, not UI changes.
- Preserve an existing Claude `statusLine` exactly and restore it only if AgentsMon still owns the setting.
- UI state changes must be broadcast to every display panel.
- Keep the no-tray product decision. The normal Dock/taskbar application presence is intentional.

## Releases

- `just release` (`./scripts/release.sh [patch|minor|major|X.Y.Z] [--no-push]`) bumps the workspace version in `Cargo.toml` and `CFBundleShortVersionString` in `bundle/Info.plist`, refreshes `Cargo.lock`, commits `release vX.Y.Z`, tags it and pushes. It builds nothing: the public artifacts only come from CI, from the tag commit. The plugin manifests keep their own versions (they version the hook contract, not the application).
- A `v*` tag runs `.github/workflows/release.yml`. macOS job (`macos-14`, arm64 only): `bundle/bundle.sh` signs the two `agentsmon-helper` copies, then the application, with the hardened runtime and `bundle/entitlements.plist`; `bundle/notarize.sh` notarises and staples the `.app`, `bundle/dmg.sh` builds the DMG, which is notarised too. Windows job: both executables, the Inno Setup installer and its uninstaller are signed with Azure Artifact Signing through GitHub OIDC (`bundle/windows/`). `workflow_dispatch` is a Windows-only dry run that publishes nothing.
- Published assets: `AgentsMon.dmg`, `AgentsMon-setup.exe` and `AgentsMon-vX.Y.Z-windows-x86_64-portable.zip`. The first two names carry no version on purpose; the in-app updater and the README links depend on them.
- Repository secrets: `CODESIGN_CERT_P12_BASE64`, `CODESIGN_CERT_PASSWORD`, `CODESIGN_IDENTITY`, `NOTARY_APPLE_ID`, `NOTARY_TEAM_ID`, `NOTARY_PASSWORD`. Variables of the `windows-release` environment (restricted to `main` and `v*` tags): `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `ARTIFACT_SIGNING_ACCOUNT`, `ARTIFACT_SIGNING_PROFILE`. The Entra application needs a federated credential for `repo:oguimbal/agentsmon:environment:windows-release`.
- A missing macOS secret gives an ad-hoc signature and a failed notarisation; missing Windows configuration fails the job. Both are intended: no unsigned public build.
- The Windows signer is identified by its EKU, never a thumbprint (Artifact Signing certificates rotate every few days). It is pinned in `bundle/windows/sign.ps1` and `crates/agentsmon/src/authenticode.rs`; keep them in sync.
- `AppId` in `bundle/windows/installer.iss` never changes. The installer file list follows `scripts/build-windows.ps1`.
- To try the release signature locally without replacing a running `dist/AgentsMon.app`: `AGENTSMON_DIST=<dir> CODESIGN_IDENTITY="<identity>" ./bundle/bundle.sh`. Notarisation is not part of local builds.

## Updates

- `crates/agentsmon/src/update.rs` reads `releases/latest` from the GitHub API at launch and every 24 hours. An update is announced only when the release is newer, is neither a draft nor a pre-release, and already has this platform's asset: the macOS and Windows jobs publish separately.
- There is no update server and no signed manifest. Downloads are accepted from this repository's release URLs only. On Windows the setup runs only if `authenticode::signed_by_oganbo` accepts it; on macOS the DMG is opened in Finder and Gatekeeper checks its notarisation.
- The arrow state (`UpdateBadge`) is application state: it goes through `broadcast` to every panel and is part of `bar_width`.
- On macOS, replacing the running bundle with a newer version makes AgentsMon restart from it; this only applies when it runs from a `.app`.
- The update check is a network request: keep the README's list of network requests accurate when it changes.

## Local source IPC

- `crates/local-source-ipc` is vendored identically in LeSwitcheur and defines the versioned contract, socket transport and golden v1 fixture; see its README. Neither repository requires a sibling checkout to build.
- `agentsmon/src/local_source.rs` publishes visible store metadata after every broadcast. Custom session/group names, states and unread notifications stay authoritative in AgentsMon. Never export conversation content or native focus targets.
- The macOS/Unix socket exists only while AgentsMon runs. Unsupported versions, malformed requests, expired commands and unavailable sessions fail safely. Windows currently exposes no local source transport.
- IPC focus, session-card focus and notification focus share `focus_agent_result`; for IPC, clear unread state only after successful focus. A user click on a card or notification clears it even when the host cannot be focused (e.g. a Warp build without session URLs). Do not add a separate activation implementation for IPC consumers.

## Manual regression checks

- Before changing the Codex desktop observer, read `docs/codex-desktop-ipc.md`. Keep incoming IPC decoding restricted to routing/status metadata; never log raw frames or Serde error messages containing input values. Preserve bounded background I/O, hook/lifecycle fallback and the persisted hourly diagnostic-notification limit.
- Before changing the OpenClaw Gateway client, read `docs/openclaw-gateway.md`. Keep the read-only scope, the field allowlist and the park-on-credential-failure policy (no retry loop on refused auth).
- Before changing Claude interruption handling, read `docs/claude-session-status.md`. Preserve explicit lifecycle timestamps, PID/session validation and fallback for older Claude processes. Silence or low CPU usage never establishes that a session is idle.

- One panel per physical display, bottom-left and always on top. On Windows the panel sits on the work area, above the taskbar; check it at a display scale other than 100%, GPUI pop-ups start with a native frame and a minimum height that `pin_bar` removes.
- Screen hotplug and resolution changes reconcile panels without duplicates.
- Dragging on one panel changes all panels; clicking the × never focuses the session.
- A right click on a card opens the rename prompt; a right click elsewhere on the bar (provider icons, empty space, update arrow) opens the bar menu, whose last entry quits AgentsMon.
- Rename a session and a group, restart AgentsMon, and check both names. Close/reopen the project, then clear each name and restart again: the cleared names must not return. See `docs/custom-names.md` for storage and recovery.
- With a release newer than the running version, the update arrow appears at the right of every bar without clipping the last card; a click downloads, then opens the DMG (macOS) or runs the signed installer (Windows).
- Context values 0, 1, 50, 99, 100 and unavailable render correctly.
- Claude states transition through idle, active, waiting and compacting.
- Plugin installation is automatic and idempotent; uninstall preserves settings edited after installation.
- macOS focus works across common terminal/IDE hosts and selects the exact Warp/iTerm2 tab (see `docs/terminal-focus.md` before adding a terminal); Windows falls back to the host window.
