#!/usr/bin/env python3
import json
from pathlib import Path

root = Path(__file__).resolve().parent.parent
marketplace = json.loads((root / ".claude-plugin/marketplace.json").read_text())
manifest = json.loads((root / "claude-plugin/.claude-plugin/plugin.json").read_text())
hooks = json.loads((root / "claude-plugin/hooks/hooks.json").read_text())

assert marketplace["name"] == "agentsmon-local"
assert len(marketplace["plugins"]) == 1
plugin = marketplace["plugins"][0]
assert plugin["name"] == manifest["name"] == "agentsmon"
assert plugin["version"] == manifest["version"]
assert (root / plugin["source"]).resolve() == (root / "claude-plugin").resolve()

required_events = {
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Stop",
    "SessionEnd",
}
assert required_events <= hooks["hooks"].keys()
for event, matchers in hooks["hooks"].items():
    assert matchers, f"{event}: missing matcher"
    for matcher in matchers:
        for hook in matcher["hooks"]:
            assert hook["type"] == "command"
            assert "${CLAUDE_PLUGIN_ROOT}/bin/agentsmon-helper" in hook["command"]
            assert hook["command"].endswith(" hook")
            assert 1 <= hook["timeout"] <= 10

print("AgentsMon Claude plugin contract is valid")

codex_plugin = root / "plugins/agentsmon-codex"
codex_manifest = json.loads((codex_plugin / ".codex-plugin/plugin.json").read_text())
codex_hooks = json.loads((codex_plugin / "hooks/hooks.json").read_text())
assert codex_manifest["name"] == "agentsmon-codex"
assert codex_manifest["hooks"] == "./hooks/hooks.json"
assert "[TODO:" not in json.dumps(codex_manifest)
codex_events = {
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "PreCompact",
    "PostCompact",
    "Stop",
}
assert codex_events == codex_hooks["hooks"].keys()
for event, matchers in codex_hooks["hooks"].items():
    assert matchers, f"Codex {event}: missing matcher"
    for matcher in matchers:
        for hook in matcher["hooks"]:
            assert hook["type"] == "command"
            assert "${PLUGIN_ROOT}/bin/agentsmon-helper" in hook["command"]
            assert hook["command"].endswith(" codex-hook")
            assert 1 <= hook["timeout"] <= 10

print("AgentsMon Codex plugin contract is valid")
