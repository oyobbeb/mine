import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

MINE = Path(__file__).resolve().parents[1] / "mine"

# Exercise the two CLI wire formats without signing in or sending project data.
FIXTURE = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
if os.environ.get("FAIL"):
    sys.exit(1)
prompt = sys.stdin.read()
with open(os.environ["TRACE"], "a") as f:
    print(json.dumps([sys.argv, prompt, os.getenv("CLAUDE_CONFIG_DIR")]), file=f)
if Path(sys.argv[0]).name == "claude":
    print(json.dumps({"session_id":"claude-session","result":"claude-answer"}))
else:
    Path(sys.argv[sys.argv.index("-o")+1]).write_text("codex-answer")
    print(json.dumps({"type":"thread.started","thread_id":"codex-session"}))
'''


class Workflow(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.project = Path(tmp.name)
        self.state = self.project / "state"
        self.trace = self.project / "trace"
        self.env = dict(os.environ, MINE_STATE_DIR=str(self.state), TRACE=str(self.trace),
                        PATH=str(self.project) + os.pathsep + os.environ["PATH"])
        for name in ("claude", "codex"):
            cli = self.project / name
            cli.write_text(FIXTURE)
            cli.chmod(0o755)

    def mine(self, *args, input=None, check=True):
        return subprocess.run([str(MINE), *args], cwd=self.project, env=self.env,
                              input=input, capture_output=True, text=True, check=check)

    def test_roundtrip_in_common_ui_and_native(self):
        self.mine("note", "fix", "never deploy")
        self.mine("ui", "fix", input="/use claude-a\nimplement\n/use codex\nreview\n/quit\n")
        room, = self.state.glob("rooms/*/fix")
        output = self.mine("_event", str(room), "codex", input=json.dumps(
            {"hook_event_name":"UserPromptSubmit","prompt":"native review"}))
        self.assertIn("claude-answer", json.loads(output.stdout)["hookSpecificOutput"]["additionalContext"])
        self.mine("_event", str(room), "codex", input=json.dumps(
            {"hook_event_name":"Stop","last_assistant_message":"native finding"}))
        self.mine("ask","fix","claude-a","continue")
        calls = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertIn("never deploy", calls[0][1])
        self.assertIn("claude-answer", calls[1][1])
        self.assertIn("codex-answer", calls[2][1])
        self.assertIn("native finding", calls[2][1])
        self.assertIn("--resume", calls[2][0])
        self.assertTrue(calls[0][2].endswith("/profiles/claude-a"))
        self.assertNotIn("native finding", self.mine("history","other").stdout)

    def test_capacity_and_failure(self):
        self.mine("ask","fix","claude-a","implement")
        self.env["FAIL"] = "1"
        self.assertNotEqual(self.mine("ask","fix","codex","review", check=False).returncode, 0)
        events = [json.loads(s) for s in self.mine("history","fix").stdout.splitlines()]
        self.assertEqual(events[-1]["role"], "error")
        self.assertIn("claude-a",self.mine("recommend","fix","continue").stdout)
        self.mine("_usage", str(self.state),"claude-a",input=json.dumps({"rate_limits":{
            "five_hour":{"used_percentage":100,"resets_at":time.time()+60}}}))
        self.assertNotIn("claude-a",self.mine("recommend","fix","continue").stdout)
        self.assertIn("codex",self.mine("recommend","fix","리뷰").stdout)


if __name__ == "__main__":
    unittest.main()
