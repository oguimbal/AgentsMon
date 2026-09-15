"""AgentsMon's local Vibe 2.25 adapter. No transcript, message or credential access.

Loaded lazily from an owned .pth in Vibe's isolated Python environment. Observe
only lifecycle discriminators and numeric statistics; preserve Vibe's calls,
return values and exceptions. An observer failure must never interrupt Vibe.
"""

import datetime
import functools
import importlib.abc
import importlib.machinery
import inspect
import json
import os
from pathlib import Path
import sys
import tempfile


def _state(status):
    return {"running": "active", "blocked": "waiting", "idle": "idle",
            "failed": "idle"}.get(status)


class Observer:
    def __init__(self):
        self.record = None
        self.compaction_id = None

    def snapshot(self, session):
        if session.parent_session_id is not None:
            return
        self._session(session.id, session.cwd)
        self.record["state"] = _state(session.status.type) or "idle"
        self._write("session/snapshot")

    def _session(self, session_id, cwd=None):
        if self.record is not None and self.record["session_id"] == session_id:
            if cwd:
                self.record["cwd"] = cwd
            return
        env = os.environ
        terminal = env.get("LC_TERMINAL") or env.get("TERM_PROGRAM")
        if env.get("ITERM_SESSION_ID"):
            terminal = "iTerm2"
        elif env.get("WT_SESSION"):
            terminal = "WindowsTerminal"
        elif env.get("GHOSTTY_RESOURCES_DIR"):
            terminal = "Ghostty"
        tmux = env.get("TMUX", "").rsplit(",", 2)
        self.record = {
            "session_id": session_id, "pid": os.getpid(), "ppid": os.getppid(),
            "cwd": cwd or os.getcwd(), "state": "idle", "source": "app_server",
            "context_tokens": None, "context_window": None,
            "host_app": terminal,
            "terminal_session_id": env.get("ITERM_SESSION_ID") or env.get("WT_SESSION"),
            "tmux_pane": env.get("TMUX_PANE"),
            "tmux_socket": tmux[0] if len(tmux) == 3 else None,
        }
        self.compaction_id = None

    def notify(self, root_id, method, params):
        # App-server also forwards child events. Only the attached root owns
        # this terminal card; a child completing cannot turn its parent grey.
        if not root_id or getattr(params, "session_id", None) != root_id:
            return
        if method in {"session/snapshot", "session/compacted", "session/contextCleared"}:
            self.snapshot(params.state.session)
            return
        if method not in {"turn/started", "turn/completed", "session/updated",
                          "session/statsUpdated", "history/entryAdded", "history/entryUpdated"}:
            return
        self._session(root_id)
        changed = False
        if method == "turn/started":
            self.record["state"] = "active"
            changed = True
        elif method == "turn/completed":
            self.record["state"] = "idle"
            self.compaction_id = None
            changed = True
        elif method == "session/statsUpdated":
            tokens, window = params.stats.context_tokens, params.context_window
            self.record["context_tokens"] = tokens if type(tokens) is int and tokens >= 0 else None
            self.record["context_window"] = window if type(window) is int and window > 0 else None
            changed = True
        elif method == "session/updated":
            for patch in params.patch:
                # Do not inspect values for title, preview, or any other path.
                if patch.path == "/status" and patch.op in {"add", "replace"}:
                    status = _state(patch.value.get("type"))
                    if status:
                        self.record["state"] = status
                        changed = True
        elif method == "history/entryAdded":
            entry = params.entry
            # Only checkpoint type/kind/id; never message, details, reasoning,
            # tool arguments, output, or any conversation-bearing field.
            if entry.type == "checkpoint" and entry.kind == "compaction":
                if entry.generation_status == "in_progress":
                    self.compaction_id = entry.id
                    self.record["state"] = "compacting"
                    changed = True
        elif method == "history/entryUpdated" and params.entry_id == self.compaction_id:
            for patch in params.patch:
                if patch.path == "/generationStatus" and patch.value == "completed":
                    self.compaction_id = None
                    self.record["state"] = "active"
                    changed = True
        if changed:
            self._write(method)

    def _write(self, event):
        self.record["event"] = event
        self.record["timestamp"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        root = Path(os.environ.get("VIBE_HOME") or Path.home() / ".vibe") / "agentsmon" / "sessions"
        root.mkdir(parents=True, exist_ok=True)
        # A separate live record prevents the coarser tool hooks from
        # overwriting lifecycle state or losing numeric context counters.
        path = root / f"{os.getpid()}.live.json"
        fd, temporary = tempfile.mkstemp(prefix=f"{os.getpid()}.", suffix=".tmp", dir=root)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump(self.record, stream)
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def _observe(server, callback):
    try:
        observer = getattr(server, "_agentsmon_observer", None)
        if observer is None:
            observer = server._agentsmon_observer = Observer()
        callback(observer)
    except Exception:
        # No logging: exceptions from foreign objects can contain user data.
        pass


def patch_server(server_class):
    if getattr(server_class, "_agentsmon_patched", False):
        return
    route = server_class._route_notification
    activate = server_class._activate_backend
    if not inspect.iscoroutinefunction(route) or not inspect.iscoroutinefunction(activate):
        return

    @functools.wraps(route)
    async def routed(self, method, params):
        _observe(self, lambda observer: observer.notify(
            getattr(self._root, "session_id", None), method, params))
        return await route(self, method, params)

    @functools.wraps(activate)
    async def activated(self, *args, **kwargs):
        result = await activate(self, *args, **kwargs)
        _observe(self, lambda observer: observer.snapshot(result.state.session))
        return result

    server_class._route_notification = routed
    server_class._activate_backend = activated
    server_class._agentsmon_patched = True


class _Loader(importlib.abc.Loader):
    def __init__(self, original):
        self.original = original

    def create_module(self, spec):
        return self.original.create_module(spec)

    def exec_module(self, module):
        self.original.exec_module(module)
        try:
            patch_server(module.AppServer)
        except Exception:
            pass


class _Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname != "vibe.app_server.server":
            return None
        spec = importlib.machinery.PathFinder.find_spec(fullname, path)
        if spec and spec.loader:
            spec.loader = _Loader(spec.loader)
            sys.meta_path.remove(self)
        return spec


def install():
    # This .pth also runs for Python maintenance commands in the Vibe venv.
    # Leave those entirely alone, including --version probes and package tools.
    entry = Path(sys.argv[0]).name
    if entry not in {"vibe", "vibe.exe", "vibe-acp", "vibe-acp.exe", "vibe-app-server", "vibe-app-server.exe"}:
        return
    if "--version" in sys.argv or "--help" in sys.argv:
        return
    if not any(isinstance(finder, _Finder) for finder in sys.meta_path):
        sys.meta_path.insert(0, _Finder())
