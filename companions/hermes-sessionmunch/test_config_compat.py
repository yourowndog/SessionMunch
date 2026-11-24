"""Focused compatibility checks; run on Sisyphus in build.slice."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

# The companion only needs these base APIs for isolated config/checkpoint tests.
base = types.ModuleType("agent.memory_provider")
base.MemoryProvider = type("MemoryProvider", (), {})
base.spawn_context_thread = lambda *a, **kw: None
sys.modules.setdefault("agent", types.ModuleType("agent"))
sys.modules["agent.memory_provider"] = base
spec = importlib.util.spec_from_file_location(
    "companion", Path(__file__).with_name("__init__.py"),
    submodule_search_locations=[str(Path(__file__).parent)])
companion = importlib.util.module_from_spec(spec)
sys.modules["companion"] = companion
spec.loader.exec_module(companion)

class ConfigCompatibility(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.env = patch.dict(os.environ, {}, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        constants = types.ModuleType("hermes_constants")
        constants.get_hermes_home = lambda: self.home
        self.mods = patch.dict(sys.modules, {"hermes_constants": constants})
        self.mods.start()
        self.addCleanup(self.mods.stop)

    def provider(self, home=None):
        p = companion.SessionMunchProvider()
        p.initialize("canary-session", hermes_home=str(home or self.home))
        return p

    def test_legacy_sidecar_and_canonical_wins_without_merge(self):
        (self.home/"ai-memory.json").write_text(json.dumps({
            "mcp_url": "http://legacy/mcp", "default_project": "historical"}))
        p = self.provider()
        self.assertTrue(p.is_available())
        self.assertEqual(p._default_project, "historical")
        (self.home/"sessionmunch.json").write_text(json.dumps({"default_project": "canonical"}))
        self.assertEqual(self.provider()._default_project, "canonical")
        self.assertNotIn("mcp_url", self.provider()._config)

    def test_env_alias_and_empty_canonical_suppresses_alias(self):
        os.environ.update(AI_MEMORY_MCP_URL="http://legacy/mcp",
                          AI_MEMORY_DEFAULT_PROJECT="historical",
                          AI_MEMORY_DEFAULT_WORKSPACE="default")
        p = self.provider()
        self.assertTrue(p.is_available())
        self.assertEqual(p._default_project, "historical")
        self.assertEqual(p._default_workspace, "default")
        os.environ.update(SESSIONMUNCH_MCP_URL="", SESSIONMUNCH_DEFAULT_PROJECT="")
        self.assertFalse(self.provider().is_available())
        self.assertEqual(self.provider()._default_project, "")
        os.environ["SESSIONMUNCH_MCP_URL"] = "http://canonical/mcp"
        self.assertEqual(companion._env("MCP_URL"), "http://canonical/mcp")

    def test_save_reads_legacy_writes_canonical_preserves_legacy(self):
        old = self.home/"ai-memory.json"
        old.write_text(json.dumps({"default_project":"historical","token_file":"private-path"}))
        before = old.read_bytes()
        utils = types.ModuleType("utils")
        utils.atomic_json_write = lambda path, value, mode: path.write_text(json.dumps(value))
        with patch.dict(sys.modules, {"utils":utils}):
            self.provider().save_config({"default_workspace":"default"}, str(self.home))
        saved = json.loads((self.home/"sessionmunch.json").read_text())
        self.assertEqual(saved["default_project"], "historical")
        self.assertNotIn("token_file", saved)
        self.assertEqual(old.read_bytes(), before)

    def test_explicit_legacy_token_file_alias(self):
        os.environ["AI_MEMORY_TOKEN_FILE"] = "/fixture/legacy-token"
        self.assertEqual(self.provider()._mcp._token_file, "/fixture/legacy-token")
        os.environ["SESSIONMUNCH_TOKEN_FILE"] = "/fixture/canonical-token"
        self.assertEqual(self.provider()._mcp._token_file, "/fixture/canonical-token")

    def test_profile_scope_and_required_checkpoint_with_active_hook(self):
        (self.home/"sessionmunch.json").write_text(json.dumps({
            "default_workspace":"default", "default_project":"historical"}))
        p = self.provider()
        calls = []
        p._mcp = types.SimpleNamespace(call_tool=lambda name, args:
                                     calls.append((name, args)) or {"content": [{"type": "text", "text": json.dumps({"path": args["path"], "page_id": "fixture-page"})}]})
        messages = [{"role":"user","content":"harmless canary evidence"}]
        before = json.dumps(messages)
        os.environ["SESSIONMUNCH_HOOK_ACTIVE"] = "1"
        receipt = p.on_pre_compress(messages, require_checkpoint=True)
        self.assertIn("checkpoints/canary-session/", receipt)
        self.assertEqual(calls[0][1]["project"], "historical")
        self.assertEqual(calls[0][1]["workspace"], "default")
        self.assertEqual(json.dumps(messages), before)
        other = self.home/"another-profile"
        other.mkdir()
        self.assertEqual(self.provider(other)._default_project, "")
        p._mcp = types.SimpleNamespace(call_tool=lambda *args: {})
        with self.assertRaises(RuntimeError):
            p.on_pre_compress(messages, require_checkpoint=True)
        p._mcp = types.SimpleNamespace(call_tool=lambda *args: {"isError":True})
        with self.assertRaises(RuntimeError):
            p.on_pre_compress(messages, require_checkpoint=True)

    def test_required_checkpoint_does_not_write_to_implicit_scope(self):
        p = self.provider()
        calls = []
        p._mcp = types.SimpleNamespace(call_tool=lambda *args: calls.append(args))
        with self.assertRaises(RuntimeError):
            p.on_pre_compress([{"role": "user", "content": "fixture"}], require_checkpoint=True)
        self.assertEqual(calls, [])

    def test_invalid_canonical_sidecar_is_not_overwritten_or_replaced_by_legacy(self):
        canonical = self.home / "sessionmunch.json"
        canonical.write_text("[]")
        (self.home / "ai-memory.json").write_text('{"default_project":"legacy"}')
        p = self.provider()
        self.assertEqual(p._default_project, "")
        utils = types.ModuleType("utils")
        utils.atomic_json_write = lambda *a, **kw: self.fail("must not write invalid sidecar")
        with patch.dict(sys.modules, {"utils": utils}):
            with self.assertRaises(ValueError):
                p.save_config({}, str(self.home))
        self.assertEqual(canonical.read_text(), "[]")

if __name__ == "__main__":
    unittest.main()
