# Claude states and interruption with Esc

Investigation and fix of 16 September 2026. This document complements
`codex-desktop-ipc.md`; both providers share the diagnostic log, the report
window and the notification limit.

## Problem and investigation results

In the Claude extension for VS Code, submitting a prompt and then interrupting
the response with Esc could leave AgentsMon green indefinitely. The Claude
process stayed alive, which is normal for an open conversation.

Two causes were identified:

1. The `Stop` hook does not run for a user interruption. This behaviour is
   documented by Anthropic; it is not a lost watcher.
2. `PostToolUseFailure` can provide `is_interrupt: true`, but the helper
   classified every tool failure as ongoing activity. This hook alone does not
   cover an interruption during thinking, before any tool.

Official sources consulted:

- [Stop](https://code.claude.com/docs/en/hooks#stop)
- [PostToolUseFailure](https://code.claude.com/docs/en/hooks#posttoolusefailure)

The last trace of the affected session was `UserPromptSubmit`, state
`active`. The Claude Status compatible file was also active; giving it
priority would therefore not have fixed the problem. No conversation text
was inspected to establish this finding.

Examining the installed code revealed a separate registry:
`~/.claude/sessions/<pid>.json`. It describes the process and, on the recent
versions observed, its current state. The registry is read by the VS Code
extension itself for its sessions open in other hosts.

| Process version observed | Result |
| --- | --- |
| 2.1.266 | File present, without `status` or `statusUpdatedAt` in the two still-open processes examined |
| 2.1.272 | Session open with `status: idle` and a timestamp; installed code contains the status publication |
| 2.1.273 | Registry and status publication; isolated real test `busy → idle` after interruption |

The version of the installed extension is not necessarily that of a
conversation already open. The affected process was still using 2.1.266
while 2.1.273 was installed. Closing and reopening the Claude session
is required to load the new binary. AgentsMon does not restart
Claude on the user's behalf.

Do not turn this observation into a guaranteed minimum version: the
first version publishing these fields has not been established. Detection is
based on the capabilities of the actual file, not on a version comparison.

## Contract read

Synthetic, simplified example:

```json
{
  "pid": 4242,
  "sessionId": "session-id",
  "startedAt": 1789590000000,
  "kind": "interactive",
  "version": "2.1.273",
  "status": "idle",
  "statusUpdatedAt": 1789590100000
}
```

Dates are Unix milliseconds. Only these fields are deserialized.
`name`, `detail`, `needs`, `logPath`, socket paths and other unknown fields
are ignored. The reader does not go through conversations and does not
connect to the inter-agent messaging socket.

| `status` | AgentsMon state |
| --- | --- |
| `busy` | Active, green; keeps Compacting if the current hook indicates it |
| `shell` | Idle, grey: turn finished, shell started in the background still alive; Awaiting if the last hook is waiting for a monitor (see below) |
| `waiting` | Waiting, orange |
| `idle` | Idle, grey |
| unknown value | Fallback to hooks, diagnostic after persistence |

`shell` is published when the turn is `idle` but at least one background
`local_bash` task is not finished (a dev server started by the agent, for
example): binary 2.1.285, `status = idle && live bash task ?
"shell" : status`. If the model were waiting for this shell, its turn would be `busy`.
Until 30 September 2026 AgentsMon displayed it in green, which left a
finished session "active" for as long as the server was running.

`updatedAt` is **not** used: a rename or another metadata change can
advance it without a status transition. The reader does not interpret
textual activity fields or values estimated by a model
either.

The registry is an internal interface, with no public stability contract and
no schema version number. `peerProtocol` describes another interface and
cannot be used to version this file.

## Waiting for a monitor or a wakeup (Awaiting)

Added on 17 October 2026 (CLI 2.1.302). A session whose turn is finished
but which has armed something to restart itself is displayed as **Awaiting**: blue-green
(`0x2a9d8f`) with a stopwatch on the context indicator. This is **not**
the case for a plain background shell.

Finding: the `Stop` hook lists `background_tasks` (`id`, `type`, `status`,
`description`, `command`) and `session_crons` (`id`, `schedule`, `recurring`,
`prompt`). In 2.1.302, `type` is `shell` for every `local_bash` task,
including those of the Monitor tool (the internal `kind: "monitor"` is not
exported). Only MCP/WebSocket monitors come out as `monitor`. The registry
publishes `shell` as soon as a `local_bash` is alive, monitors included. A dev server and a
Monitor are therefore indistinguishable by their type, and a `Bash` started with
`run_in_background` notifies the agent when it ends exactly like a dev server:
waiting for a build started this way is **not detectable** without reading the
conversation, and stays grey.

Rule applied by the helper:

1. `PostToolUse` of the `Monitor` tool without `agent_id`: second read of the hook
   limited to `tool_response.taskId` and `persistent`. A non-persistent monitor
   adds its (opaque) id to the record's `monitor_task_ids` (16 at most).
   Persistent monitors (endless log watching) are ignored.
2. `Stop`: only the `id` of tasks and the `recurring` of crons are decoded.
   Monitor ids absent from `background_tasks` are forgotten. If the turn
   was going to become Idle and a one-shot cron (`recurring: false`:
   `ScheduleWakeup`, single `CronCreate` reminder) exists, the state is
   `awaiting` / activity `wakeup`; otherwise, if a retained monitor is still running,
   `awaiting` / `monitor`. Recurring crons (`/loop 5m`…) stay grey.
3. The `idle_prompt` reminder and subagent hooks are merged as
   heartbeats on an Awaiting record.

On the provider side, a more recent `shell` registry keeps Awaiting; `idle`
keeps it only for a wakeup (no process: the registry publishes
`idle` at the end of the turn; its order relative to the `Stop` hook has not been
measured, both cases are covered), and ends it for a monitor (task
stopped without restarting the turn). `busy` and `waiting` win as usual.

Verified live on 17 October 2026 with a temporary hook logging only
the ids/types: `PostToolUse` Monitor → `{"taskId":"bfxllzues","timeoutMs":120000,"persistent":false}`,
then `Stop` → `background_tasks: [{"id":"bfxllzues","type":"shell","status":"running"}]`.

The Active → Awaiting transition does not trigger the end-of-turn notification;
Awaiting → Idle/Waiting triggers it. The local v1 protocol does not have this state:
it is exported as `active`.

Limits: a single `CronCreate` reminder far in the future (tomorrow at 15:00) leaves the card
Awaiting until then; a cron deleted from the interface, without a tool or a hook,
leaves it Awaiting until the next prompt. A WebSocket/MCP monitor is not a `local_bash`, so the registry
then publishes `idle` and the card goes back to grey.

## Event order and safeguards

The provider always discovers sessions from the `.cstatus` files and validates
the Claude process. The registry only complements recognized sessions.
For each live session:

1. Read only `sessions/<pid>.json`, in the sibling directory of the provider's
   `projects` root.
2. Check the PID and the session identifier. A session resumed elsewhere or
   an old file for a reused PID must not modify the card.
3. Check `startedAt` against the process start time obtained through
   `sysinfo`. Files from a previous process generation are ignored.
4. Accept `kind: interactive` or its absence; ignore the known categories
   `bg`, `daemon`, `daemon-worker`. A new category triggers a diagnostic.
5. Check that `statusUpdatedAt` is neither earlier than `startedAt` nor more
   than two seconds later than the current time.
6. Apply the status only if `statusUpdatedAt` is strictly more recent
   than the last hook. A prompt sent after an old `idle` therefore becomes
   active again immediately.

The helper now records `event_timestamp` separately from `timestamp`.
A status line refresh can advance `timestamp` without changing
`event_timestamp`, the state or the last event. On the first refresh
of an old record, its old `timestamp` is used as the starting point. A real
hook advances both dates. Its empty activity also clears the old tool name.

`PostToolUseFailure.is_interrupt: true` goes straight back to Idle if the hook does not
carry an `agent_id`. The interruption of a subagent does not by itself prove
the end of the main turn. An ordinary failure or a hook from an old version
without this field stays Active.

A hook carrying an `agent_id` (`SubagentStart`, `SubagentStop`, tools of a
subagent…) that would compute Active on a record already Idle is merged as
a heartbeat: state, activity, last event and `event_timestamp` are
kept, and a more recent registry status still applies.
Permission requests from a subagent stay Waiting, and a subagent
during an active turn changes nothing.

Observed on 30 September 2026 on a CLI 2.1.285 session that stayed green: registry
`idle`, then `SubagentStop` about three minutes later, with no matching
transcript in `subagents/`. The binary contains forked internal agents
without a transcript (`away_summary`, `extract_memories`, `auto_dream`…); these are
possible sources, the exact emitter has not been established. The AgentsMon state just
before this hook, overwritten since, could not be verified.

The same case occurred again on 30 September 2026 ("devops" session, CLI
2.1.285, Warp host) despite the heartbeat merge: the record was not Idle
when `SubagentStop` arrived, because the `Stop` hook had written `active`
("background task") because of background shells. Two fixes:

- `Stop` stays Active only if `background_tasks` contains agent
  work (`subagent`, `workflow`, `teammate`, `cloud session`). `shell`,
  `monitor`, `MCP task`, `dream` or `auto-mode scan` tasks alone give Idle
  (or Awaiting for a monitor or a wakeup, see the dedicated section). Only
  the `type` field of each task is decoded; `command` and `description`
  contain text and are not read.
- On the provider side, when the last hook is `SubagentStart`/`SubagentStop`, an
  `idle`/`shell` registry applies even if it is older than this hook. A
  resumed main turn republishes `busy`, which remains subject to the normal rule.

Same mechanism on 1 October 2026 on a VS Code 2.1.281 session ("edf"):
registry `waiting` (question asked), then `SubagentStop` four minutes
later, and the card went back to green. The safeguard therefore also covers waiting:

- on the provider side, a `waiting` registry also applies over a more recent
  `SubagentStart`/`SubagentStop` hook;
- on the helper side, the record remembers whether its last hook came from a subagent
  (`subagent`). An Active subagent hook is merged as a heartbeat on an
  Idle record, or a Waiting one **from the main turn**. A permission request
  issued by a subagent always ends when that subagent resumes.

`Notification` hook: only `permission_prompt`, `worker_permission_prompt`,
`elicitation_dialog`, `elicitation_url_dialog` and `agent_needs_input` (or a
notification without a type, old versions) give Waiting. `idle_prompt`,
sent one minute after a finished turn left unanswered, gives Idle;
records written before 1 October 2026 with this type are also read as Idle.
`auth_success` stays Active. The other types (`agent_completed`,
`push_notification`, `quota_auto_resume_*`, `computer_use_*`,
`model_refusal_fallback`, future types) do not change the state: they are
merged as a heartbeat. List taken from binary 2.1.285.

Known limit: if such an agent requests a permission after the end of the turn,
the card becomes Waiting, then its next `PostToolUse` sets it back to Active.

The data stays in the Claude provider and the helper. The store, the GPUI
rendering, the other panels and the local export receive the same `AgentSnapshot`
as for the other sources. No Claude behaviour is added to the UI.

## Resilience and limits

- Maximum 256 KiB per file, including if it grows during the read.
- Regular files only; symbolic links, directories and FIFOs are refused.
  Unix adds `O_NOFOLLOW | O_NONBLOCK` on open to close the race with
  a replacement by a link or a FIFO. On Windows, the initial check and
  the handle check are applied; this additional Unix protection does not exist
  in this Windows implementation.
- Reads are small synchronous local reads, with no external
  command, network wait or internal polling loop. Like the `.cstatus` files,
  they nevertheless depend on the responsiveness of the local file system.
- Partial JSON, access error, incompatible field, inconsistent timestamp: no
  panic, no old `idle` kept in cache, fallback to the last hook.
- A missing file is not an incident: old versions, startup and
  normal shutdown can all explain this absence. A silent disappearance
  of the whole registry therefore cannot be distinguished from a normal case.
- A file present without a status for a non-Idle session produces an explicit
  diagnostic after the delay below. It may be an old version,
  a disabled feature or a change of format.
- The watcher also observes the `sessions` directory if it exists. The
  ordinary ten-second refresh covers its later creation or
  a watcher failure. When the watcher works, changes are grouped
  with the other events (refresh at most once per second).
- No silence delay, CPU usage or tool execution time means
  "interrupted". A long computation stays active.
- The reader keeps a conservative time order: a more recent hook wins
  over an older registry. A late hook emitted after Idle was published
  may therefore need a new registry transition to be corrected.
- Local clocks are assumed consistent; inconsistent values are
  ignored or reported, without inventing a transition.
- Registry support does not make WSL sessions detectable from
  Windows; this provider limit remains.

## Diagnostics and notifications

The same problem must be observed for at least fifteen seconds before
being exposed to the application. This delay filters out, in particular, files
truncated during an in-place write. A valid read or the absence of a
problem resets this delay to zero. If several sessions are affected, the
first one in the stable order of identifiers is reported; the others are
examined as well and keep their individual fallback.

| Code | Interpretation |
| --- | --- |
| `claude.registry.schema` | JSON or required fields incompatible/incomplete |
| `claude.registry.read` | Access/read error or growth beyond the limit |
| `claude.registry.file` | File type or initial size refused |
| `claude.registry.kind` | Unknown session category |
| `claude.registry.status` | Unknown status value |
| `claude.registry.timestamp` | Inconsistent order or range of dates |
| `claude.registry.status_unavailable` | File without a complete state for a non-Idle session; restart Claude with a recent version |

Serde errors can contain a value taken from the input: they are
never copied into the logs. Diagnostics contain fixed codes and texts,
the file path and the PID. For the case without a status, the version can
be added only if it has at most 32 characters, all digits
or dots. No JSON dump, conversation name or Claude error message
is stored.

The shared mechanism `connection_diagnostics.rs`:

- explicitly logs the incident, its persistence and its disappearance;
- keeps the reports and the last notification attempt on disk;
- limits diagnostic notifications to **one per hour in total**, Claude
  and Codex combined, including after a restart;
- opens a copyable report when the notification is clicked; the menu
  **AgentsMon → Connection diagnostics…** also opens the reports.

On macOS, the files are located in
`~/Library/Application Support/fr.gmbl.AgentsMon/diagnostics/`:
`provider-diagnostics.log`, its `.1` archive, and `provider-diagnostics.json`.
The path actually used is stated in the report. The log rotates at
512 KiB. System permissions can prevent a native notification;
the logs and the menu remain usable. See `codex-desktop-ipc.md` for the
delivery and click limits of notifications.

## Finding the contract again after an update

1. Read the version from the file of the **affected process**, not only
   from `claude --version` or the extension's `package.json`.
2. Examine only the metadata fields listed above in
   `~/.claude/sessions/<pid>.json` and the corresponding AgentsMon record. Do not
   search for `[Request interrupted by user]` in the messages: this approach
   would break the confidentiality rule and would depend on a display text.
3. In `~/.vscode/extensions/anthropic.claude-code-<version>-<platform>/extension.js`,
   search for `liveRegistry`, `session_state_changed`, `sessions`, `busy`,
   `waiting`. The installed reader observed recognizes four states:
   `busy`, `shell`, `idle`, `waiting`.
4. In `resources/native-binary/claude`, the embedded JavaScript observed is
   still visible. Look for `statusUpdatedAt`, the publication of
   `{status: ..., waitingFor: ...}`, then its callers. Minified names change.
   In 2.1.273, the observed function `Bfe` adds the dates and writes the registry;
   `Hh` links the headless lifecycle to the `busy`/`waiting`/`idle` values.
5. The extension sends a control request `{subtype: "interrupt"}` when
   a session is interrupted. The isolated test below reproduces this boundary
   without touching a user session.
6. Adapt only the typed reader and its tests. A schema change does not
   justify deserializing the whole file into `serde_json::Value` or
   replacing the explicit status with an inactivity timeout.

## Reproducible verification

Deterministic tests:

```sh
cargo test -p agentsmon-providers claude_runtime
cargo test -p agentsmon-helper
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

The tests cover interruption without Stop, the next prompt, the four
statuses, compaction, prolonged silence, incorrect identifiers/PIDs,
process generations, excluded categories, old versions,
invalid dates, truncated JSON, oversized data, links/FIFOs and the
diagnostic/recovery delay. The helper also checks the interruption field,
subagents, ordinary failure and status line heartbeats.

Test with the real installed Claude binary (opt-in):

```sh
python3 scripts/test-claude-interruption.py --claude /path/to/claude
```

The script creates a temporary configuration, uses `--bare`, a fake key
and an HTTP server on `127.0.0.1`. The server keeps an SSE response open,
the script waits for `busy`, sends the interruption control, then waits for `idle`.
Tools, plugins, MCP and session persistence are disabled. The parent's
authentication and provider selection variables are not inherited.
The process is stopped in `finally`, even if the test fails. No account
or real model API is used; only status metadata is
printed. Raw Claude output is ignored.

Run successfully on 16 September 2026 on the binary of the VS Code extension
2.1.273: `busy → idle` during a synthetic model request. This validates the
headless interruption boundary used by VS Code; it is not a GUI
test of the routing of the Esc key.

Manual check after deployment: restart AgentsMon, reopen the Claude
session with the recent binary, send a prompt, interrupt during
thinking, then send a new prompt. Expect grey after the interruption,
green at the new prompt. Repeat during a tool and a permission request.
Do not interrupt a user session automatically to test.

In a CLI session, finish a turn, check grey, then leave the terminal
without interaction for more than three minutes: the card must stay grey.
