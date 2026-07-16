# AgentsMon

AgentsMon is a small, always-on-top desktop monitor for local coding agents. It is written in Rust with [GPUI](https://www.gpui.rs/) and currently supports Claude Code and Codex on macOS and Windows.

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

At launch, AgentsMon automatically installs or repairs the user-scoped Claude integration when needed. An existing compatible Claude Status hook is recognized, in which case AgentsMon only adds the context indicator integration. The application menu keeps a diagnostic screen for inspecting or disabling the integration.

The helper records only session metadata in `~/.claude/projects/*/*.cstatus`. For context usage, it receives Claude Code's official `context_window.used_percentage` status-line field. If a status line already exists, AgentsMon stores it and proxies its input/output instead of replacing its visible behavior.

Uninstall restores the previous status line only while the configured command still points to AgentsMon. A user edit made after installation always wins.

## Codex integration

AgentsMon adds idempotent user-level Codex lifecycle hooks while preserving every existing hook definition. Codex requires the user to review and trust new hooks before they run. New prompts, tools, approval requests, compaction and completed turns update the same provider-neutral session model used by Claude.

The helper examines only Codex `token_count` metadata records to calculate context usage. It does not deserialize or store user and assistant messages. Clicking a Codex card opens the exact task through the native `codex://threads/<session-id>` deep link.

## Workspace

| Crate | Role |
| --- | --- |
| `agentsmon-core` | Provider-neutral models and in-memory ordering/hiding store |
| `agentsmon-providers` | Provider trait, Claude/Codex discovery, watchers and OS focus |
| `agentsmon-ui` | GPUI bars, context ring, drag/drop and diagnostics |
| `agentsmon-helper` | Claude/Codex hook and status-line helper executable |
| `agentsmon` | Application lifecycle, windows, menus and onboarding |

The Claude plugin and local marketplace live in `claude-plugin/` and `.claude-plugin/`. The Codex plugin lives in `plugins/agentsmon-codex/` and is included in release bundles.

## Privacy

AgentsMon does not parse message content. It reads `.cstatus` metadata, lifecycle hook metadata and context-window metrics needed for session identity and context percentage. Nothing is uploaded.
