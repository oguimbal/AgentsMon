# Focusing a session in its terminal

Investigation of 5 October 2026. A click on a card (or a notification,
or an IPC request) must bring to the foreground **the tab** that hosts the
session, not just the application. This document lists what each
terminal allows, what AgentsMon does today and the approaches ruled out.

## Common chain

1. The helper (`crates/agentsmon-helper/src/record.rs`, `EnvironmentMetadata`)
   reads the hook's environment: `host_app` (`LC_TERMINAL` / `TERM_PROGRAM`),
   `terminal_session_id`, `TMUX_PANE` / `TMUX`. It records only these
   identifiers, never content.
2. The provider builds a `FocusTarget`; `find_host` walks up the process
   tree to find the host's PID.
3. `crates/agentsmon-providers/src/focus.rs`, `focus()`: a provider URI
   (`FocusTarget::uri`: VS Code, Cursor, Claude.app, Codex) goes first,
   then tmux (`select-window` + `select-pane`) if present, then `focus_host`:
   host-specific selection (iTerm2, then Warp) and, as a last resort,
   activation of the application by PID (`focus_pid`).

For a terminal to be targetable tab by tab, it needs **both**:
a per-tab/pane identifier visible in the shell's environment, and an
external API that selects this identifier. Targeting by working
directory is ambiguous (two sessions in `~/work/api`, for example).

## Summary

| Terminal | Identifier in the env | Selection API | Vertical tabs | "Quake"-style window | AgentsMon status |
|---|---|---|---|---|---|
| Warp | `WARP_TERMINAL_SESSION_UUID`, `WARP_FOCUS_URL` | `warp://session/<uuid>` | ✅ | ✅ (dedicated hotkey) | ✅ tab |
| iTerm2 | `ITERM_SESSION_ID` | AppleScript (`select`, `reveal hotkey window`) | ✅ (bar on the left) | ✅ (hotkey window) | ✅ tab, except a hidden hotkey window |
| cmux | `CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID` | CLI / socket `cmux select-workspace`, `focus-panel` | ✅ | ⚠️ global show/hide | ❌ app only |
| Ghostty 1.3 | none per terminal | AppleScript (`focus`) | ❌ | ✅ quick terminal | ❌ app only |
| WezTerm | `WEZTERM_PANE` | `wezterm cli activate-pane` | ❌ | ❌ | ❌ app only |
| kitty | `KITTY_WINDOW_ID` | `kitten @ focus-window` (remote control) | ❌ | ✅ quick-access-terminal | ❌ app only |
| Terminal.app | `TERM_SESSION_ID` | AppleScript (tabs by `tty`) | ❌ | ❌ | ❌ app only |
| VS Code / Cursor | — | `vscode://file/…`, `cursor://file/…` | — | — | folder |
| Windows Terminal | `WT_SESSION` | none | ❌ | ✅ | window only |

## Warp (implemented)

- Warp has no AppleScript dictionary. Its accessibility tree contains
  only a text area and the window buttons, without the tabs.
- Since the resolution of [warpdotdev/warp#8611](https://github.com/warpdotdev/warp/issues/8611)
  (closed as *completed* on 22 May 2026), each session exports
  `WARP_TERMINAL_SESSION_UUID` (32 hex digits) and
  `WARP_FOCUS_URL=warp://session/<uuid>`. Observed with
  `WarpTerminal v0.2026.09.09.08.26.stable_02`.
- The helper records the UUID as `terminal_session_id` **only if
  `TERM_PROGRAM=WarpTerminal`**: under tmux, or in an application launched
  from Warp that inherits the variables, the UUID does not designate the real host.
- `focus_host` builds `warp://session/<uuid>` itself (identifier restricted
  to hex digits and `-`, 64 characters at most) instead of opening a URL read
  from a file. On failure, it falls back to activation by PID.
- Sessions started before the helper update have no UUID: their
  next hook adds it.
- Ruled out: `warp.sqlite` (Group Container `2BBY89MBSN.dev.warp`) does contain
  `windows` / `tabs` / `terminal_panes`, but it is a restoration
  snapshot (4 old tabs versus 11 open during the test). The database also contains
  the block history, hence commands and their outputs: do not read it.
- Ruled out: simulating `Cmd+N` or the hotkey's key. There is no reliable tab
  index, and the hotkey toggles (it would hide an already visible window).

Manual check: from another Warp tab,
`sleep 5; open "$WARP_FOCUS_URL_OF_THE_SESSION"`, then hide Warp with the hotkey.
Warp must reappear on the correct tab.

## iTerm2 (implemented, to be completed)

- `ITERM_SESSION_ID` (`w0t1p0:<uuid>`) is captured. The AppleScript script of
  `focus_iterm_session` walks `windows → tabs → sessions`, compares
  `unique ID`, then calls `select` on the tab and the window and `activate`.
- The tab bar can be placed on the left (Settings → Appearance →
  Tab bar location), and the dedicated "hotkey window" accepts tabs.
- Still to do: a hidden hotkey window is not revealed by `activate`. The
  dictionary (`iTerm2.sdef`) exposes `is hotkey window` on the window and the
  `reveal hotkey window` command, which would need to be called in that case.

## cmux (not implemented, best alternative candidate)

- [cmux](https://github.com/manaflow-ai/cmux) is a macOS terminal based on
  Ghostty, with a vertical sidebar designed for agents.
- Each terminal exports `CMUX_WORKSPACE_ID` and `CMUX_SURFACE_ID`. The
  [CLI](https://cmux.com/docs/api) (socket `/tmp/cmux.sock`, or
  `CMUX_SOCKET_PATH`) offers `select-workspace --workspace <id>` and
  `focus-panel --panel <id>`.
- The documentation does not say whether these commands bring the window to the
  foreground: plan for `focus_pid` afterwards, as for iTerm2.
- No real "quake" mode. There is a global shortcut
  `showHideAllWindows` ([configuration](https://cmux.com/docs/configuration)).
- Implementation: capture `CMUX_SURFACE_ID` (and the workspace) in the helper,
  then add a cmux branch in `focus_host` that calls the CLI with a
  validated identifier.

## Ghostty 1.3 (not implemented)

- The [AppleScript support](https://ghostty.org/docs/features/applescript)
  (`Ghostty.sdef`, 1.3.0) exposes `windows → tabs → terminals`, with for each
  terminal `id`, `name` and `working directory`, as well as the `focus`,
  `select tab` and `activate window` commands. It wraps the App Intents (Shortcuts).
- Blocker: no environment variable identifies the terminal (the binary
  only knows `GHOSTTY_RESOURCES_DIR`, `GHOSTTY_BIN_DIR`,
  `GHOSTTY_SHELL_FEATURES`, `GHOSTTY_QUICK_TERMINAL`…). Only targeting by
  `working directory` is possible, which is ambiguous when several sessions share
  a folder. The AppleScript `id` is not visible from the shell.
- No vertical tabs ([discussion #2549](https://github.com/ghostty-org/ghostty/discussions/2549)).
  The quick terminal is not exposed through AppleScript.

## WezTerm, kitty, Terminal.app (not implemented)

Rows taken from each project's documentation, not verified on a
machine (none of the three was installed during the investigation).

- WezTerm: `WEZTERM_PANE` then `wezterm cli activate-pane --pane-id <id>`.
  Reliable targeting, but neither native vertical tabs nor a quake window.
- kitty: `KITTY_WINDOW_ID` then `kitten @ focus-window --match id:<id>`.
  Requires `allow_remote_control` and a `listen_on` known to AgentsMon. The
  `quick-access-terminal` kitten provides the quake mode. No vertical tabs.
- Terminal.app: the AppleScript dictionary exposes the `tty` of each tab.
  The helper would have to record the tty of the Claude process.

## Constraints

- Never read conversation content or terminal output to
  find a tab (block titles, history, restoration databases).
- Any value that comes from a status file and is passed to `open`, `osascript` or
  a CLI must be validated (restricted alphabet, bounded length) and passed
  as an argument, never interpolated into a script.
- An IPC request marks a session as read only after a successful focus
  (`focus_agent_result`). A user click marks it as read even if
  the focus fails.
