# AgentsMon

AgentsMon is a small, always-on-top desktop monitor for local coding agents. It is written in Rust with [GPUI](https://www.gpui.rs/) and currently supports Claude Code, Codex and Mistral Vibe on macOS and Windows.

Each physical display gets the same low-profile bar in its bottom-left corner. Every running session shows:

- the basename of its working directory;
- green, orange, grey, or blue state feedback;
- a circular context-window gauge around the state dot;
- drag-and-drop ordering and a temporary hide action;
- click-to-focus for the host terminal or IDE.

There is no tray icon, analytics, conversation storage, or session history.

## Status

This repository contains the v0.1 implementation:

- extensible `AgentProvider` interface, multi-provider aggregation and in-memory `AgentStore`;
- `.cstatus`-compatible Claude provider with PID validation and filesystem watching;
- native Claude hook/status-line helper;
- native Codex lifecycle hooks with context metrics and deep-link focusing;
- Mistral Vibe lifecycle events, context usage and terminal focusing;
- guided plugin installation that preserves an existing status line;
- GPUI multi-display bars and macOS/Windows focus backends;
- macOS application bundling and Windows release staging.

Linux is intentionally not packaged yet. Windows support targets native Claude Code with Git Bash; WSL-hosted sessions are not resolved to Windows windows in v0.1.

## Build

Use the stable Rust toolchain. GPUI's current dependencies require a recent stable compiler.

```sh
cargo check --workspace
cargo test --workspace
cargo run -p agentsmon
```

Build the helper before testing the onboarding flow locally:

```sh
cargo build -p agentsmon-helper
cargo run -p agentsmon
```

On macOS:

```sh
./bundle/bundle.sh
open dist/AgentsMon.app
```

On Windows:

```powershell
./scripts/build-windows.ps1
```

## Claude integration

At launch, AgentsMon automatically installs or repairs the user-scoped Claude integration when needed. Existing compatible status plugins can remain installed; AgentsMon adds its own metadata-only lifecycle hooks alongside them so completed turns and approval states are tracked reliably. The application menu keeps a diagnostic screen for inspecting or disabling the integration.

The helper records only session metadata in `~/.claude/projects/*/*.cstatus`. For context usage, it receives Claude Code's official `context_window.used_percentage` status-line field. If a status line already exists, AgentsMon stores it and proxies its input/output instead of replacing its visible behavior.

Uninstall restores the previous status line only while the configured command still points to AgentsMon. A user edit made after installation always wins.

## Codex integration

AgentsMon adds idempotent user-level Codex lifecycle hooks while preserving every existing hook definition. Codex requires the user to review and trust new hooks before they run. New prompts, tools, approval requests, compaction and completed turns update the same provider-neutral session model used by Claude.

The helper examines Codex `token_count` metadata records to calculate context usage. It does not deserialize or store user and assistant messages.

Codex permission requests remain green for a three-second grace period. Only a request still pending at a later refresh becomes orange and can trigger an attention notification; short-lived requests never enter the shared waiting state. A genuine pending request appears after the grace period on the next refresh (normally within about 13 seconds of its hook), even if no further hook arrives. Other waiting states, completion, and compaction are not delayed.

The provider also checks recent `task_started`, `task_complete`, and `turn_aborted` lifecycle metadata to recover missed completion hooks. Silent reasoning never expires on a timer; a newer completion or interruption ends the active state. Lifecycle checks run on the normal refresh cycle (at most 10 seconds), inspect at most the last 512 KiB, and decode only event types and timestamps. If the transcript is unavailable or contains no recent lifecycle marker, the last hook state remains authoritative until another event or process exit. Existing status records gain the transcript path on their next hook after updating the helper.

Clicking a Codex card opens the exact task through the native `codex://threads/<session-id>` deep link.

## Mistral Vibe integration

At launch, AgentsMon detects Mistral Vibe 2.18 or newer and installs three user-level hooks in `~/.vibe/hooks.toml`, preserving existing hooks and TOML comments. Vibe 2.18–2.20 uses the experimental hook names; AgentsMon enables `enable_experimental_hooks` when the option is absent. An explicit `false` is preserved: enable the option yourself to connect those versions. Vibe 2.21+ uses the stable hook names. Restart Vibe after installation or a version upgrade, and relaunch AgentsMon after upgrading Vibe to migrate its hooks.

For Vibe 2.25+ installed in an isolated Python environment (including `uv tool install mistral-vibe`), AgentsMon also installs `_agentsmon_vibe.py` and `_agentsmon_vibe.pth` into that environment. This local adapter observes Vibe's app-server lifecycle notifications. A plain response turns green from the beginning, permission and question waits turn orange, completion/interruption returns to idle, and compaction is tracked. Context usage uses Vibe's numeric `context_tokens / context_window`, whose denominator is its configured automatic compaction threshold. This matches Vibe's own context budget, rather than an assumed model capacity.

**Restart Vibe once after installation; `/reload` only reloads hooks, not the Python adapter.** Restart AgentsMon after upgrading Vibe so it can repair the adapter in the new environment. The adapter is lazy, leaves maintenance Python commands alone, and never changes Vibe's return values or exceptions. It uses internal app-server boundaries, tested against Vibe 2.25.4; incompatible versions fall back to hooks with a warning in the AgentsMon log. Older versions and standalone installations only report tool/completion hooks and cannot track initial reasoning, normal permission waits, interruptions or context. No activity is inferred from CPU usage or a slow tool.

Clicking a card focuses its host terminal, including iTerm sessions and tmux panes. A new conversation in the same Vibe process replaces the previous card, and exited processes disappear on refresh. Child events do not overwrite the parent's state. Only session identity, working directory, lifecycle state, numeric context counters, timestamps and process/terminal metadata are written to `~/.vibe/agentsmon/sessions/`. Live records take precedence over the coarser hook records. Session titles, previews, messages, tool inputs/outputs, summaries, transcripts and credentials are never read by this adapter. `VIBE_HOME` is supported when set consistently for both AgentsMon and Vibe. Project-level hook overrides take precedence for the hook fallback.

Account quota remains unavailable: Vibe's app-server does not expose remaining subscription allowances. Mistral's [Admin API](https://docs.mistral.ai/admin/admin-api/usage-metrics) provides organization usage and spending/rate limits with a separate Admin API key; it is not connected by this local integration. Session token counts are not presented as account quotas.

To disconnect, quit AgentsMon to prevent automatic repair, remove the three `agentsmon-*` entries from `hooks.toml` and the two `_agentsmon_vibe` files from Vibe's Python site-packages, then restart Vibe. Other hook definitions can remain in place.

Run `python3 scripts/test-vibe-observer.py` for the adapter tests. Run it with Vibe's environment Python to additionally exercise the actual Vibe beginning-of-turn code and notification routing without calling a model or accessing a conversation.

The integration follows Mistral's [hook definitions](https://github.com/mistralai/mistral-vibe/blob/main/vibe/core/hooks/models.py) and [2.21 hook migration](https://github.com/mistralai/mistral-vibe/blob/main/CHANGELOG.md#2210---2026-07-17).

## Workspace

| Crate | Role |
| --- | --- |
| `agentsmon-core` | Provider-neutral models and in-memory ordering/hiding store |
| `agentsmon-providers` | Provider trait, Claude/Codex/Vibe discovery, presentation assets, watchers and OS focus |
| `agentsmon-ui` | GPUI bars, context ring, drag/drop and diagnostics |
| `agentsmon-helper` | Claude/Codex/Vibe hook and status-line helper executable |
| `agentsmon` | Application lifecycle, windows, menus and onboarding |

The Claude plugin and local marketplace live in `claude-plugin/` and `.claude-plugin/`. The Codex plugin lives in `plugins/agentsmon-codex/` and is included in release bundles.

## Privacy

AgentsMon does not parse message content. It reads `.cstatus` metadata, lifecycle hook metadata and context-window metrics needed for session identity and context percentage. Nothing is uploaded.
