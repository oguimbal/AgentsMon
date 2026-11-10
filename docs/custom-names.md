# Persistent custom names

Since the fix of 17 September 2026, the names entered in the AgentsMon
"Rename" window are persistent preferences. Previously, they
were held in memory only and were also deleted when the session
disappeared from a provider's snapshot.

## Behaviour

- A session name is associated with the provider / session identifier pair.
  It comes back when the same session is resumed, even after a restart.
  A new conversation with a different identifier keeps its own name.
- A group name is associated with its grouping key, currently the path
  of the working directory. It applies when a group forms again in that
  directory, even with new conversations. A different path designates
  a different group; no automatic matching of paths is done.
- A group reduced to a single session becomes a session card, in line
  with the existing rendering. Its group name stays saved and comes back as soon as the
  group contains several members again.
- Preferences survive empty snapshots, hidden sessions and
  periods when no agent is running. They do not expire automatically.
- Submitting an empty name removes the saved customization and restores the
  project name used by default. Cancelling the window changes nothing.
- The rename is immediately broadcast to all panels and to the local IPC,
  as before. Names remain managed by AgentsMon; they do not rename
  the conversation in Claude or Codex.

## Storage

On macOS:
`~/Library/Application Support/fr.gmbl.AgentsMon/custom-names.json`.
On other platforms, use the `data_dir` of
`directories::ProjectDirs::from("fr", "gmbl", "AgentsMon")`.

Synthetic example:

```json
{
  "version": 1,
  "sessions": [
    {
      "id": { "provider": "claude", "session_id": "session-id" },
      "name": "API tests"
    }
  ],
  "groups": [
    {
      "key": { "Directory": "/path/to/project" },
      "name": "Local project"
    }
  ]
}
```

Only the names entered in AgentsMon and their keys are saved. No
snapshot, activity state, provider-supplied title, message, PID or focus
target goes into this file. Hidden sessions keep their separate
file `hidden-sessions.json`. Card order stays in memory.

`AgentStore` exposes the sorted preferences through `custom_names()` and restores them
before the first snapshot through `restore_custom_names()`. It does no I/O.
`agentsmon/src/custom_names.rs` handles reading and writing at the
application level; the rendering and the providers do not change.

## Durability and errors

The save happens during the broadcast that follows the rename, before
returning to event processing. It does not depend on a close
callback: killing the process after submitting must not lose the name.
Subsequent broadcasts write nothing when the preferences are unchanged.

Each write uses a unique temporary file, then `sync_all` and an
atomic replacement. On Unix, the file is created with mode `0600` and the
directory is synced if the file system allows it. On Windows,
the temporary handle is closed before `MoveFileExW` with replacement and
`WRITE_THROUGH`. A failed write does not replace the last valid file.

A save error leaves the names in memory and writes an explicit
warning to the logs. The same content is retried after at least thirty
seconds during a broadcast; a new change can retry
immediately. This avoids attempts and logs on every refresh.
A persistent disk error can prevent names from surviving a restart; it
does not prevent AgentsMon from working.

Reading and writing are limited to 4 MiB. A missing file means
no customization. Invalid JSON, an unknown version or a read
error is logged without copying its values. The original file stays
intact during startup and refreshes. On the first new
rename, it is moved to `custom-names.json.recovery.<timestamp>` before
the new preferences are written. If this move fails, the write is
abandoned and will be retried; the original is not overwritten.

To examine a missing customization, check the file, the full session
identifier and the group key. To recover a backup, stop
AgentsMon, inspect the `.recovery.*` file, restore only the desired
names in the v1 schema, then restart. Old names already lost before
this fix was installed did not exist on disk and cannot
be restored from this file.

## Checks

Automated tests:

```sh
cargo test -p agentsmon-core
cargo test -p agentsmon custom_names
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

They check restoration after an empty snapshot, isolation between
providers, group re-formation, clearing of names, a restart
simulated immediately after a save, Unicode characters/quotes,
invalid or future files, their preservation, the size limit and
recovery after a write failure.

Manual check: rename a card and a group, restart AgentsMon, check
the labels on the panels and in the LeSwitcheur results; close then
resume a session; finally clear both names, restart and check that
the customizations do not reappear.
