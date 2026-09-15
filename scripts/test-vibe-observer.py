"""Run with Python 3.11+, or Vibe's Python for the real app-server contract test."""

import asyncio
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True

SOURCE = Path(__file__).resolve().parents[1] / "crates/agentsmon/src/vibe_observer.py"
spec = importlib.util.spec_from_file_location("observer", SOURCE)
observer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observer)


class Private:
    """Fail if the adapter reads any content-bearing attribute."""
    def __getattr__(self, name):
        raise AssertionError(f"Conversation content accessed: {name}")


class ObserverTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.environment = patch.dict(os.environ, {"VIBE_HOME": self.directory.name})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.observer = observer.Observer()
        self.path = Path(self.directory.name) / "agentsmon/sessions" / f"{os.getpid()}.live.json"

    def saved(self):
        return json.loads(self.path.read_text())

    def emit(self, method, **fields):
        self.observer.notify("root", method, NS(session_id="root", **fields))

    def snapshot(self, session_id="root"):
        session = NS(id=session_id, parent_session_id=None, cwd="/tmp/demo",
                     status=NS(type="idle"), preview=Private(), title=Private())
        self.observer.snapshot(session)

    def test_plain_response_starts_before_any_tool_and_completes(self):
        self.snapshot()
        self.assertEqual(self.saved()["state"], "idle")
        self.emit("turn/started", turn=Private())
        self.assertEqual(self.saved()["state"], "active")
        self.emit("history/entryAdded", entry=NS(type="reasoning", text=Private()))
        self.assertEqual(self.saved()["state"], "active")
        self.emit("turn/completed", turn=Private())
        self.assertEqual(self.saved()["state"], "idle")

    def test_permission_wait_resume_and_interruption(self):
        for status, expected in [("running", "active"), ("blocked", "waiting"),
                                 ("running", "active"), ("idle", "idle"), ("failed", "idle")]:
            self.emit("session/updated", patch=[NS(path="/preview", op="replace", value=Private()),
                      NS(path="/status", op="replace", value={"type": status})])
            self.assertEqual(self.saved()["state"], expected)

    def test_context_counters_survive_other_events_and_reset_on_session_switch(self):
        self.emit("session/statsUpdated", stats=NS(context_tokens=50_000), context_window=100_000)
        self.emit("turn/completed", turn=Private())
        self.assertEqual(self.saved()["context_tokens"], 50_000)
        self.assertEqual(self.saved()["context_window"], 100_000)
        self.snapshot("new-session")
        self.assertIsNone(self.saved()["context_tokens"])
        for tokens, window in [(0, 100), (99, 100), (100, 100), (200, 100), (1, 0)]:
            self.emit("session/statsUpdated", stats=NS(context_tokens=tokens), context_window=window)
            self.assertEqual(self.saved()["context_tokens"], tokens)
            self.assertEqual(self.saved()["context_window"], window or None)

    def test_subagents_and_unknown_events_cannot_overwrite_parent(self):
        self.emit("turn/started")
        before = self.path.read_bytes()
        self.observer.notify("root", "turn/completed", NS(session_id="child"))
        self.observer.notify("root", "unknown", NS(session_id="root", content=Private()))
        self.observer.snapshot(NS(parent_session_id="root"))
        self.assertEqual(self.path.read_bytes(), before)

    def test_compaction_metadata_without_reading_summary(self):
        self.emit("history/entryAdded", entry=NS(type="checkpoint", kind="compaction",
                  id="compact", generation_status="in_progress", message=Private(), details=Private()))
        self.assertEqual(self.saved()["state"], "compacting")
        self.emit("history/entryUpdated", entry_id="compact", patch=[
            NS(path="/message", value=Private()),
            NS(path="/generationStatus", value="completed")])
        self.assertEqual(self.saved()["state"], "active")

    def test_hooks_cannot_overwrite_observer_record(self):
        self.emit("turn/started")
        hook = self.path.parent / f"{os.getpid()}.json"
        hook.write_text('{"state":"idle"}')
        self.assertEqual(self.saved()["state"], "active")
        self.assertFalse(any(self.path.parent.glob("*.tmp")))

    def test_observer_failure_does_not_change_vibes_result_or_exception(self):
        class Server:
            _root = NS(session_id="root")
            async def _route_notification(self, method, params):
                if method == "failure":
                    raise ValueError("upstream failure")
                return "original result"
            async def _activate_backend(self):
                return "original activation result"

        observer.patch_server(Server)
        observer.patch_server(Server)
        server = Server()
        with patch.object(observer.Observer, "notify", side_effect=OSError("disk full")):
            self.assertEqual(asyncio.run(server._route_notification("turn/started", None)), "original result")
        self.assertEqual(asyncio.run(server._activate_backend()), "original activation result")
        with self.assertRaisesRegex(ValueError, "upstream failure"):
            asyncio.run(server._route_notification("failure", None))

    def test_maintenance_python_does_not_install_import_hook(self):
        for arguments in [["-c"], ["pip"], ["vibe", "--version"], ["vibe", "--help"]]:
            with patch.object(sys, "argv", arguments):
                before = sys.meta_path[:]
                observer.install()
                self.assertEqual(sys.meta_path, before)

    def test_real_vibe_app_server_plain_turn_and_permission_events(self):
        if importlib.util.find_spec("vibe") is None:
            self.skipTest("Run with Vibe 2.25's Python for the app-server contract test")
        from vibe.app_server.server import AppServer
        from vibe.app_server._turns import TurnController
        from vibe.app_server.models import AgentStatsSnapshot, JsonPatchOperation, PublicTurn, PublicTurnStatus
        from vibe.app_server.protocol import SessionUpdatedParams, StatsUpdatedParams, TurnCompletedParams
        observer.patch_server(AppServer)
        sent = []

        class Transport:
            async def send(self, value):
                sent.append(value)

        server = object.__new__(AppServer)
        server._root = NS(session_id="root")
        server._client_capabilities = NS(disabled_notifications=set())
        server._attaching = False
        server._connection_attached = True
        server._transport = Transport()
        turn = PublicTurn(id="turn", session_id="root", status=PublicTurnStatus.IN_PROGRESS, started_at=1)

        async def no_op(*args):
            pass

        async def run():
            # Execute Vibe's actual beginning-of-turn code, before any model
            # response or tool hook exists. This was missing from the old tests.
            handler = NS(_notify=server._route_notification, _emit_status=no_op,
                         _emit_stats=no_op, _agent_loop=NS(stats=NS(context_tokens=0)))
            await TurnController._announce_turn_started(handler, turn)
            self.assertEqual(self.saved()["state"], "active")
            await server._route_notification("session/statsUpdated", StatsUpdatedParams(
                session_id="root", event_id=1, emitted_at=1,
                stats=AgentStatsSnapshot(context_tokens=50_000), context_window=100_000))
            self.assertEqual(self.saved()["context_tokens"], 50_000)
            await server._route_notification("session/updated", SessionUpdatedParams(
                session_id="root", event_id=1, emitted_at=1,
                patch=[JsonPatchOperation(op="replace", path="/status", value={"type": "blocked"})]))
            self.assertEqual(self.saved()["state"], "waiting")
            await server._route_notification("turn/completed", TurnCompletedParams(
                session_id="root", event_id=2, emitted_at=2,
                turn=turn.model_copy(update={"status": PublicTurnStatus.INTERRUPTED})))
            self.assertEqual(self.saved()["state"], "idle")

        asyncio.run(run())
        self.assertEqual([message["method"] for message in sent],
                         ["turn/started", "session/statsUpdated", "session/updated", "turn/completed"])


if __name__ == "__main__":
    unittest.main()
