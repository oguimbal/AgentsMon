# Codex desktop: wait states and IPC protocol diagnostics

This document describes an **internal** interface, observed on ChatGPT macOS
**26.903.71938, build 8576**, with its Codex executable **0.153.4**, on
16 September 2026. It is not a compatibility guarantee from OpenAI.
Do not raise an accepted version without checking the contract and the tests.

## Why hooks are not enough

The initial case was a Computer Use approval: "Allow ChatGPT to use
LeSwitcheurPreviewLight?". Codex displayed "Awaiting approval" and AgentsMon
stayed green. During this block, independent checks found:

| Source | Observation |
| --- | --- |
| AgentsMon record | `state=active`, `event=PreToolUse`, `activity=mcp__node_repl__js` |
| Rollout | Last write at 18:45:23 UTC; no progress during the wait |
| `node_repl/active_execs/*.json` | Execution present, but no approval indicator |
| `logs_2.sqlite` | Last technical event attributed to the task at 18:45:54 UTC; no new waiting hook |
| SQLite and desktop global state | No usable waiting status found for this task |
| Desktop IPC, tested live | `threadRuntimeStatus={type:"active",activeFlags:["waitingOnApproval"]}` |
| Request metadata, investigation probe | `method=mcpServer/elicitation/request` |
| Active control task | `activeFlags=[]`, no pending request |

The integration probe then found `idle`, consistent with the task's `Stop`
hook after it resumed/ended. A long `node_repl` must therefore not be turned
automatically into a user wait. A long computation produces the same
silence signals on disk.

The [official MCP event](https://learn.chatgpt.com/docs/app-server#mcp-server-elicitation-requests)
is `mcpServer/elicitation/request`. Its existence explains the behavior, but
the app-server documentation **does not document** the Electron transport used here.

### A lead wrongly ruled out during the initial investigation

The desktop's app-server processes use `stdio://`. Starting another
app-server or the daemon does not give access to the in-memory state of these processes.
However, `~/.codex/ipc/ipc.sock` is an Electron router between local clients.
A newline-delimited JSON-RPC probe gets no valid
response: that is not its format. This socket **does allow** discovering the owner
of a task and receiving its current state with the protocol below.

## Code layout

| File | Responsibility |
| --- | --- |
| `crates/agentsmon-providers/src/codex_ipc/protocol.rs` | Versions, restricted schemas, validation and state translation |
| `crates/agentsmon-providers/src/codex_ipc/transport.rs` | Unix socket, ownership check, frames, limits and timeouts |
| `crates/agentsmon-providers/src/codex_ipc/mod.rs` | Worker, sampling, owner discovery, cache and fallback |
| `crates/agentsmon-providers/src/codex_ipc/tests.rs` | Fake router, fault, privacy, cache and optional live probe |
| `crates/agentsmon-providers/src/codex.rs` | Overlay of the direct status on hooks/lifecycle |
| `crates/agentsmon/src/connection_diagnostics.rs` | Persistent reports, rotating log, global notification limit |
| `crates/agentsmon/src/notification.rs` | Native notification and capture of the report for the click |
| `crates/agentsmon-ui/src/diagnostic_report.rs` | Generic window and prompt copy |

The core and `MonitorView` do not know the Codex protocol. Worker
changes go through `AgentProvider::take_changed()`, then the same `refresh` /
`broadcast` as hooks: every panel and the AgentsMon IPC source receive
the new state. The IPC **published by AgentsMon** is a different contract: do not
modify `crates/local-source-ipc` to accommodate Codex's private protocol.

## Verified transport and sequence

- Unix endpoint: `$CODEX_HOME/ipc/ipc.sock`, or `~/.codex/ipc/ipc.sock`.
- AgentsMon never creates, deletes or repairs this socket.
- Private directory owned by the current user; private socket of the same
  user. Symbolic links and group/other permissions are refused.
- Each message: **4 bytes u32 little-endian**, the length of the UTF-8 JSON that follows,
  then exactly that many bytes. No line separator.
- `type` values include `request`, `response`, `broadcast`,
  `client-discovery-request`, `client-discovery-response`.
- The `requestId` values correlate responses; `sourceClientId` and `targetClientIds`
  route messages. `hostId=local` only.

| Message | Verified version |
| --- | ---: |
| `initialize` | 0 |
| `thread-owner-discovery` | 1 |
| `thread-stream-following-changed` | 1 |
| `thread-stream-state-changed` | 11 |

### 1. Initialization

```json
{"type":"request","requestId":"agentsmon-123-1","sourceClientId":"initializing-client","version":0,"method":"initialize","params":{"clientType":"agentsmon"},"timeoutMs":2000}
```

The response must have `resultType=success`, `method=initialize` and
a UUID `result.clientId`. Use this identifier for the following messages.
An unknown client is allowed; do not impersonate a Codex window.

### 2. Task owner discovery

```json
{"type":"request","requestId":"agentsmon-123-2","sourceClientId":"CLIENT_UUID","version":1,"method":"thread-owner-discovery","params":{"hostId":"local","conversationId":"THREAD_UUID"},"timeoutMs":2000}
```

The successful response provides `handledByClientId`. `error=no-client-found` is a
normal case: CLI task, closed window, unloaded task or task without a desktop
owner. Other contract errors produce a diagnostic.

### 3. Temporary subscription and snapshot

```json
{"type":"broadcast","sourceClientId":"CLIENT_UUID","targetClientIds":["OWNER_UUID"],"method":"thread-stream-following-changed","version":1,"params":{"conversationId":"THREAD_UUID","hostId":"local","following":true}}
```

The owner publishes `thread-stream-state-changed`. **Synthetic** example
reduced to the fields of interest; the real message contains other fields that
must never be decoded or recorded:

```json
{
  "type": "broadcast",
  "method": "thread-stream-state-changed",
  "sourceClientId": "OWNER_UUID",
  "version": 11,
  "params": {
    "conversationId": "THREAD_UUID",
    "hostId": "local",
    "change": {
      "type": "snapshot",
      "revision": 252,
      "conversationState": {
        "id": "THREAD_UUID",
        "threadRuntimeStatus": {
          "type": "active",
          "activeFlags": ["waitingOnApproval"]
        }
      }
    }
  }
}
```

Check the sender, the version, the host and both task identifiers. Do not
accept a status from another task. A `change.type=patches` is ignored:
wait for the full snapshot. Its values are not parsed.

### 4. Unsubscription

Send exactly the subscription message with `following=false`, on success
**and on error**. If sending fails, close the connection. Disconnecting gets
the client unregistered at the router. The temporary subscription accepts no
approval, does not mark the task as read and requests no role change.
The desktop may keep its state loaded during this brief subscription.

The router may ask whether AgentsMon can handle commands via
`client-discovery-request`. Reply `client-discovery-response` with
`response.canHandle=false`. Never offer a command handler or claim
ownership of a task.

## States, freshness and limits

| Direct status | Result |
| --- | --- |
| `active` + `waitingOnApproval` | `Waiting` / orange |
| `active` + `waitingOnUserInput` | `Waiting` / orange |
| `active` + empty array | `Active` / green, except that an explicit compaction is kept |
| `idle` | `Idle` / grey |
| `notLoaded`, `systemError` | No override; use hooks/lifecycle |
| Unknown state or flag, `activeFlags` missing for `active` | Diagnostic and fallback |

- Single worker per provider, no network wait on the GPUI thread.
- Eligible sessions have a valid process and a non-idle fallback state.
  Archived/expired sessions are excluded before this step.
- At most **32 sessions**, the most recently active first; the others
  keep the hooks. Provider constructors intended for file tests
  open no connection to the real desktop.
- Nominal sampling every **3 seconds**, woken when the targets
  change. One connection is reused; the owner is rediscovered at each
  sample to support a window change.
- **3 seconds per operation** (connection, init, discovery, full read,
  subscribe/unsubscribe). The timeout is global for the frame: a sender that
  provides only a few bytes cannot extend the read indefinitely.
- Targets are queried sequentially: under heavy load, the frequency
  per target may drop. Freshness takes priority over keeping an
  old status.
- Cache valid for **less than 15 seconds**; rejected as soon as the hook timestamp
  changes. A new hook cannot inherit the approval of the previous turn.
  Explicit end events detected by the provider keep priority.
- Updates are batched and displayed through the usual refresh (at most
  once per second on signal; periodic refresh normally 10 seconds).
  A silent expiry therefore appears at the next periodic refresh.
- Frames limited to **16 MiB** (the application allows more). Fallible
  allocation, JSON bounded by the frame, limited Serde recursion, at most 16 flags and
  128 interleaved messages before a response. A larger snapshot causes a
  documented fallback, not an automatic increase of the limit.
- After an error: cache invalidated, connection dropped. Gradual reconnection
  from 6 to 60 seconds; suspected incompatibility: 60 seconds. Hooks do not
  bypass this delay. No aggressive reconnection loop.
- A worker panic is caught; the cache is discarded and a diagnostic is
  reported. No blocking `join` when the interface shuts down.
- Direct support on Unix; **Windows stays on hooks/lifecycle**. Do not announce
  Windows named pipe support without the corresponding implementation and tests.

A missing socket, its closure and a missing owner are not
evidence of a protocol change. They do not generate a compatibility
notification. A change that makes every discovery indistinguishable from a
`no-client-found` cannot be identified with certainty by this contract alone.

## Privacy

The desktop's wire format potentially carries an entire conversation.
The worker receives the bounded bytes in memory for the duration of decoding, then
frees them. It decodes only the envelope, the routing identifiers and
`threadRuntimeStatus`. Titles, previews, turns, messages, requests,
tool arguments/results and patches are ignored by Serde.

`RawValue` is only a **borrowed reference** into the temporary frame,
used to choose the schema after the envelope has been checked. Never keep
a copy of it, print it, write it to disk, or use `serde_json::Value` for
a real response. `Value` is reserved for requests built by AgentsMon
and for synthetic test data.

Serde errors can include input values in their text. This is
why only their category, line and column are logged. Unknown
status/flag values are not reproduced in the report.

## Notifications and diagnostic files

A detected incident produces:

1. An explicit `agentsmon::codex_ipc` event in tracing, with code, phase,
   versions, limits and fallback.
2. A JSON event in **`diagnostics/provider-diagnostics.log`**, even if
   `RUST_LOG` hides warnings or Finder does not keep stderr.
3. A report in **`diagnostics/provider-diagnostics.json`**, with the dates,
   the last incident of each provider and the notification reservation.
4. At most **one notification per hour**, globally for these diagnostics,
   across all codes/providers. This limit is persisted **before** sending,
   and still applies after a restart, a change of error and a recovery.

On macOS, the directory is normally:

```text
~/Library/Application Support/fr.gmbl.AgentsMon/diagnostics/
```

The log rotates at **512 KiB**, with a single previous `.log.1`. Identical
repetitions are logged at most once per hour; a new incident or
an incident that stops being reported is logged immediately. The incident
notification limit does not apply to ordinary agent notifications.

The click opens a report with **"Copy diagnostic prompt"**. The prompt
includes the available expected/observed versions, the code, the phase, the
timestamps, the socket, the technical session ID, the log paths and this
document. It is not sent automatically. It is also available in
**AgentsMon → Connection diagnostics…**, even after a restart or if
native notifications are disabled.

The callback captures the report that was sent: a later repair does not replace
the notification details with "all is well". The diagnostic
notification has no short application-level TTL: it stays clickable in the notification
center while AgentsMon is running. The system keeps control
of banner display. After the application stops, the in-memory native
handlers are no longer available; use the persistent report from the menu.

If the diagnostics folder is not writable, a warning says so. The
limit stays active in memory but its persistence across restarts cannot
be guaranteed. A clock moving backwards does not trigger repeated notifications.
The report says "incident no longer reported" when an incident disappears; this
is not evidence that the desktop is connected (it may be stopped).

## Procedure when Codex changes

### Reading the incident codes

| Code | First check |
| --- | --- |
| `stream_version_changed` | Version of `thread-stream-state-changed` in the Electron client's table |
| `schema_changed` | Structure of the envelope/snapshot at the indicated phase; only the JSON category and position are kept |
| `initialize_rejected`, `initialize_result_missing`, `invalid_client_id` | Initialization response and format of `result.clientId` |
| `owner_response_changed`, `invalid_owner_id` | Owner discovery, version 1 and `handledByClientId` |
| `snapshot_missing`, `snapshot_state_missing`, `session_mismatch` | Local routing and the `params.change.conversationState` path |
| `unknown_change_type` | New variants of `change.type`; do not interpret a patch as a snapshot |
| `missing_active_flags`, `unknown_active_flag`, `unknown_runtime_status` | Status and flag enum; check their semantics before accepting them |
| `frame_size` | Little-endian framing and observed size; possibly a conversation that is too large, with no protocol change |
| `response_timeout` | Phase concerned, desktop temporarily blocked or expected response changed |
| `message_limit` | Routing too broad, stray stream, or expected snapshot that does not arrive |
| `allocation_limit` | Memory limits and snapshot size; do not remove the bounds |
| `worker_start_failed`, `worker_panicked`, `encode_error` | Local AgentsMon problem; inspect the environment and the worker |
| `transport_unavailable` | Socket missing/closed/refused; normal fallback, without a protocol notification |

### Investigation

1. Copy the prompt from the notification or the menu. Keep the technical
   logs; do not copy a conversation, a full rollout or a DB.
2. Examine `code`, `phase`, `expected` / `observed`, the frame size and
   the IO error. Distinguish unknown version, missing schema, timeout and missing socket.
3. Check the version of the application (`Contents/Info.plist`) and of the embedded
   binary (`Contents/Resources/codex --version`). The `codex` command on the PATH
   may be another version, or even a broken npm installation.
4. Inspect **the application code**, not the conversation data. In
   `/Applications/ChatGPT.app/Contents/Resources/app.asar`, the files of the
   initial version were:
   - `.vite/build/src-B6LqG3ek.js`: `IpcClient`, `IpcRouter`, framing and versions;
   - `webview/assets/app-initial-a9514281e192.js`: `threadRuntimeStatus`,
     `handleThreadStreamFollowingChanged`, `sendConversationSnapshot`;
   - `.vite/build/main-Bkkz0ENj.js`: connections to the desktop/app-server.
   The suffixes change with builds. Search for the stable strings
   `ipc.sock`, `readUInt32LE`, `thread-owner-discovery`,
   `thread-stream-following-changed`, `thread-stream-state-changed`,
   `waitingOnApproval`, `mcpServer/elicitation/request`.
5. To extract an ASAR without an npm dependency: read the four little-endian u32
   at offset 0; the fourth is the length of the index JSON at offset 16.
   The data starts at `8 + second_u32`. Walk `files` in the index;
   file offsets are relative to this start. Write only the selected JavaScript
   files to a temporary folder. Do not modify the app.
6. Check the sequence above, the names/fields and the new versions in
   the code. Revise the typed decoders and the synthetic tests before accepting
   a new version. A different number does not prove compatibility.
7. Use the explicit probe below with a UUID obtained from an AgentsMon
   metadata record. Observe an approval that is actually displayed and an
   active control task; never accept/deny an approval in order to probe.
8. Get the fault and privacy tests to pass, then the workspace
   checks. Update this document with the new verified version and the
   differences observed.

## Tests and commands

```sh
cargo test -p agentsmon-providers codex
cargo test -p agentsmon connection_diagnostics
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
claude plugin validate .
python3 scripts/validate-plugin.py
```

The IPC tests create only temporary sockets and synthetic
data. A sandbox environment may forbid `bind`/`connect` with
`EPERM`: allow these local tests, do not disable their assertion to make
them pass. The live probe is ignored by default:

```sh
AGENTSMON_CODEX_DIAGNOSTIC_SESSION=EXPLICIT_UUID \
  cargo test -p agentsmon-providers live_desktop_status -- --ignored --nocapture
```

It prints only the supplied ID and `Some(Waiting)`, `Some(Active)`, `Some(Idle)`
or `None`. A fault prints only a sanitized `Fault`. It unsubscribes and
keeps neither the snapshot nor the messages.

The tests cover: states and flags, unknown schema/version, ignored private
payload, different source/task, ignored patches, refusal to offer commands,
unsubscription on error, missing owner/socket, fragmentation, excessive
size, blocked partial read, EOF, stale cache/new hook, worker,
hourly limit after restart/recovery, rotating log and inaccessible
storage. Native notifications and copying the report also require
a check of the interface on the target platform.

### Isolated check of the window

```sh
cargo run -p agentsmon --example diagnostic_preview
```

This executable opens the real `DiagnosticReportView` with a synthetic
report marked **LOCAL TEST**. It starts no provider, installs no
hook, displays no bar and does not touch the real diagnostics folder.
Check scrolling, resizing and "Copy diagnostic prompt".
The full text is copied, even when some lines are outside the visible
area. This test was performed on macOS during development of the fix.

To also test native delivery and the click, add `--notify` to **this example**.
On macOS, the binary must be placed in a `.app` bundle with a
`CFBundleIdentifier` and the notification permission. Use a separate test
identity so as not to replace the handler of the open AgentsMon instance.
The example uses the same `NativeNotifications` as the application; its click
reopens the captured report. The notification explicitly carries the name **Codex —
TEST**. It does not consume the hourly limit of the real diagnostics folder.

### Initial validation of the fix

- The 109 automated tests of the workspace pass on macOS (two tests ignored
  by default: Vibe fixture subprocess and Codex live probe).
- `cargo check --workspace`, Clippy with `-D warnings`, `cargo fmt --check`,
  `claude plugin validate .` and `scripts/validate-plugin.py` pass.
- The probe in the integrated code communicates with the installed application and finds
  `Idle` for the initial task, which had emitted `Stop` in the meantime.
- The initial investigation had measured `waitingOnApproval` on this same task
  during the block; the fake routers cover this state and the drift to a
  version 12, up to the publication of the diagnostic by the worker.
- The window and its copy button were checked in the test bundle.
  Display/click of a real native notification and Windows remain
  separate manual checks; do not infer them from a quota test.
