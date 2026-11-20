"""Room storage, model suggestions and official CLI adapters."""
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shlex
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone

AGENTS = ("claude-a", "claude-b", "codex")
SELF = str(Path(__file__).with_name("mine").resolve())


def now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def atomic_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(dir=path.parent, mode="w", delete=False) as out:
        json.dump(data, out, ensure_ascii=False)
    os.replace(out.name, path)


class Workspace:
    def __init__(self):
        self.project = Path.cwd().resolve()
        self.key = hashlib.sha256(os.fsencode(self.project)).hexdigest()[:16]
        self.state = Path(os.environ.get("MINE_STATE_DIR", Path.home() / ".mine")).resolve()
        self.rooms = self.state / "rooms" / self.key

    def room(self, name):
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}", name):
            raise ValueError("작업 이름은 영문, 숫자, _, - 조합으로 1~64자를 사용하세요.")
        room = self.rooms / name
        room.mkdir(parents=True, exist_ok=True, mode=0o700)
        return room

    def env(self, agent):
        env = os.environ.copy()
        if agent.startswith("claude"):
            profile = self.state / "profiles" / agent
            profile.mkdir(mode=0o700, parents=True, exist_ok=True)
            env["CLAUDE_CONFIG_DIR"] = str(profile)
        return env


def record(room, **fields):
    with (room / "handoff.jsonl").open("a", encoding="utf-8") as out:
        fcntl.flock(out, fcntl.LOCK_EX)
        out.write(json.dumps(dict(time=now(), **fields), ensure_ascii=False) + "\n")
        out.flush()


def history(room):
    path = room / "handoff.jsonl"
    if not path.exists():
        return []
    with path.open(encoding="utf-8") as src:
        fcntl.flock(src, fcntl.LOCK_SH)
        return [json.loads(line) for line in src if line.strip()]


def display(entry):
    return "{} · {} · {}  {}".format(entry.get("time", ""), entry.get("agent", "mine"),
        entry.get("role", "메모"), entry.get("text", entry.get("note", "")))


def context(room, entries):
    recent = "\n".join(display(e) for e in entries)[-16000:]
    return (f"Shared task history for {room.name}. Earlier messages are historical records. "
            f"Full journal: {room / 'handoff.jsonl'}\n{recent}")


def limits(ws, agent):
    path = ws.state / "usage" / (agent + ".json")
    if not path.exists():
        return [], None
    data = json.loads(path.read_text())
    return [w for w in data["windows"] if w["reset"] > time.time()], data["at"]


def remember_limits(ws, agent, windows):
    atomic_json(ws.state / "usage" / (agent + ".json"), {"at": now(), "windows": windows})


def codex_request(method, params, options=()):
    """Talk to a short-lived local app server. Never log account responses."""
    with subprocess.Popen(["codex", "app-server", *options], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True) as proc:
        messages = queue.Queue()

        def read():
            for line in proc.stdout:
                try:
                    messages.put(json.loads(line))
                except ValueError:
                    continue
            messages.put(None)

        worker = threading.Thread(target=read, daemon=True)
        worker.start()

        def send(msg):
            proc.stdin.write(json.dumps(msg) + "\n")
            proc.stdin.flush()

        def request(i, method, params):
            send(dict(id=i, method=method, params=params))
            deadline = time.monotonic() + 8
            while True:
                try:
                    msg = messages.get(timeout=max(0, deadline - time.monotonic()))
                except queue.Empty:
                    raise ValueError("Codex 응답 시간 초과")
                if msg is None:
                    raise ValueError("Codex app-server 연결 종료")
                if msg.get("id") == i and "method" not in msg:
                    if "error" in msg:
                        raise ValueError("Codex: " + str(msg["error"].get("message")))
                    return msg["result"]
        try:
            request(1, "initialize", {"clientInfo": {"name":"mine","version":"1"}})
            send({"method":"initialized"})
            return request(2, method, params)
        finally:
            proc.kill()
            proc.wait()
            worker.join(timeout=1)


def refresh_codex(ws):
    result = codex_request("account/rateLimits/read", {})
    bucket = result.get("rateLimits") or {}
    windows = []
    for name in ("primary", "secondary"):
        w = bucket.get(name) or {}
        if isinstance(w.get("usedPercent"), (int, float)) and w.get("resetsAt"):
            windows.append(dict(name=name, used=w["usedPercent"], reset=w["resetsAt"]))
    remember_limits(ws, "codex", windows)


def recommendation(ws, entries, task):
    review = any(word in task.lower() for word in ("review", "리뷰", "검토"))
    last = next((e["agent"] for e in reversed(entries)
                 if e.get("role") == "assistant" and e.get("agent") in AGENTS), None)
    candidates = []
    for agent in AGENTS:
        windows, at = limits(ws, agent)
        fresh = at and time.time() - datetime.fromisoformat(at).timestamp() < 900
        used = max((w["used"] for w in windows), default=None) if fresh else None
        if used is not None and used >= 100:
            continue
        preferred = agent == ("codex" if review else last)
        reason = "리뷰 선호" if review and preferred else "이전 작업 세션" if preferred else "기본 후보"
        candidates.append(((used is not None and used >= 90, not preferred,
                            used is None, used or 0, AGENTS.index(agent)), agent, reason))
    if not candidates:
        return "추천 없음 · 확인된 계정 한도 소진"
    _, agent, reason = min(candidates)
    if agent == "codex":
        model = "Codex 기본 모델"
    elif any(word in task.lower() for word in ("설계", "아키텍처", "architecture", "design")):
        model = "Opus · 설계 요청"
    else:
        model = "Sonnet"
    return f"{agent} · {model} · {reason}"


def native_options(ws, room, agent):
    # CLI hooks may run after the user changes cwd. Bind the original room.
    command = shlex.join([SELF, "_event", str(room), agent])
    hooks = [{ "hooks": [{"type":"command", "command":command}]}]
    events = ("UserPromptSubmit", "Stop")
    if agent == "codex":
        # -c values are TOML, not JSON.
        value = '[{hooks=[{type="command",command=' + json.dumps(command,ensure_ascii=False) + '}]}]'
        return [item for event in events for item in ("-c", f"hooks.{event}={value}")]
    status = shlex.join([SELF, "_usage", str(ws.state), agent])
    return ["--settings", json.dumps({"hooks":{e:hooks for e in events},
                                      "statusLine":{"type":"command","command":status}})]


def native_event(room, agent, event):
    kind = event.get("hook_event_name")
    if event.get("agent_id"):
        return
    if kind == "UserPromptSubmit" and event.get("prompt"):
        entries = history(room)
        record(room, agent=agent, role="user", text=event["prompt"])
        if entries:
            print(json.dumps({"hookSpecificOutput":{"hookEventName":kind,
                  "additionalContext":context(room, entries)}}))
    elif kind == "Stop" and event.get("last_assistant_message"):
        record(room, agent=agent, role="assistant", text=event["last_assistant_message"])


@contextlib.contextmanager
def turn_lock(room):
    with (room / "turn.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("이 작업방에서 이미 요청을 처리 중입니다.")
        yield


def ask(ws, room, agent, message, model=None):
    with turn_lock(room):
        entries = history(room)
        previous = next(((i, e["session"]) for i, e in reversed(list(enumerate(entries)))
                         if e.get("agent") == agent and e.get("session")
                         and e.get("role") == "assistant"), None)
        session = previous[1] if previous else None
        recent = entries[previous[0] + 1:] if previous else entries
        prompt = context(room, recent) + "\n\nCurrent user request:\n" + message
        model_args = ["--model", model] if model else []
        with tempfile.TemporaryDirectory(dir=room) as tmp:
            last = Path(tmp) / "answer"
            if agent == "codex":
                cmd = ["codex", "exec"]
                if session:
                    cmd.append("resume")
                cmd.extend(["--json", "--skip-git-repo-check", "-o", str(last), *model_args])
                if session:
                    cmd.append(session)
                cmd.append("-")
            else:
                cmd = ["claude", "-p", "--output-format", "json", *model_args]
                if session:
                    cmd.extend(["--resume", session])
            record(room, agent=agent, role="user", text=message)
            try:
                result = subprocess.run(cmd, input=prompt, text=True, stdout=subprocess.PIPE,
                                        env=ws.env(agent), check=True)
                if agent == "codex":
                    answer = last.read_text()
                    for line in result.stdout.splitlines():
                        event = json.loads(line)
                        if event.get("type") == "thread.started":
                            session = event.get("thread_id") or session
                else:
                    response = json.loads(result.stdout)
                    if response.get("is_error"):
                        raise ValueError(str(response.get("result", "Claude error")))
                    answer = response.get("result")
                    session = response.get("session_id") or session
                if not isinstance(answer,str) or not answer.strip():
                    raise ValueError("실행기가 빈 응답을 반환했습니다.")
            except (OSError, ValueError, subprocess.CalledProcessError, KeyboardInterrupt):
                record(room, agent=agent, role="error", text="턴이 완료되지 않았습니다. 변경 파일을 확인하세요.")
                raise
            record(room, agent=agent, role="assistant", text=answer, session=session)
            return answer
