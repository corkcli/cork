"""Cork for Hermes: the same hooks Claude Code, Codex and Cursor run, plus
teammates' messages delivered into the open conversation.

A thin shim. Every decision is made by the `cork` program
(~/.cork/bin/cork), exactly as for the other agents; this file only
hands Hermes's hook calls to it and its answers back. Installed and kept up
to date by `cork install`; removed by `cork uninstall`.

A machine installed before the rename (Collide to Cork) keeps its
~/.collide folder and ~/.collide/bin/collide; both are found.
"""

import json
import os
import subprocess
import threading
from pathlib import Path

_listeners = {}
_briefed = set()
_lock = threading.Lock()


def _home():
    return os.environ.get("CORK_HOME") or os.environ.get("COLLIDE_HOME") or str(Path.home())


def _data_dir():
    """~/.cork, or the ~/.collide of a machine set up before the rename for
    as long as no ~/.cork exists beside it (the program picks the same)."""
    cork, collide = Path(_home()) / ".cork", Path(_home()) / ".collide"
    return collide if (not cork.exists() and collide.exists()) else cork


def _program():
    ext = ".exe" if os.name == "nt" else ""
    for name in ("cork", "collide"):
        path = _data_dir() / "bin" / f"{name}{ext}"
        if path.exists():
            return str(path)
    return ""


def _cwd():
    try:
        from agent.runtime_cwd import resolve_agent_cwd
        return str(resolve_agent_cwd())
    except Exception:
        return os.getcwd()


def _usage_path(session_id):
    """Where this session's model calls are written down, one line each, in
    the shape Claude Code's transcript gives them: Cork bills a Hermes
    session from this file the way it bills any transcript."""
    if not session_id:
        return ""
    safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in session_id)[:120]
    return str(_data_dir() / "hermes" / f"usage-{safe}.jsonl")


def _record_usage(session_id="", api_request_id="", usage=None, model="", response_model="", **kwargs):
    """post_api_request: one provider call's tokens. Hermes's `usage` is its
    CanonicalUsage: `input_tokens` is uncached input (cache reads and writes
    are their own buckets), `output_tokens` includes reasoning."""
    path = _usage_path(session_id)
    if not path or not isinstance(usage, dict):
        return
    count = lambda key: max(0, int(usage.get(key) or 0))
    line = {
        "type": "assistant",
        "message": {
            "id": f"hermes:{api_request_id or kwargs.get('started_at') or ''}",
            "model": response_model or model or "",
            "usage": {
                "input_tokens": count("input_tokens"),
                "output_tokens": count("output_tokens"),
                "cache_read_input_tokens": count("cache_read_tokens"),
                "cache_creation_input_tokens": count("cache_write_tokens"),
            },
        },
    }
    try:
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        with _lock, open(path, "a", encoding="utf-8") as out:
            out.write(json.dumps(line) + "\n")
    except Exception:
        pass


def _call(command, event, kwargs, timeout=30):
    program = _program()
    if not program:
        return {}
    args = kwargs.get("args")
    payload = {
        "hook_event_name": event,
        "tool_name": kwargs.get("tool_name"),
        "tool_input": args if isinstance(args, dict) else None,
        "tool_response": kwargs.get("result") if isinstance(kwargs.get("result"), str) else None,
        "session_id": kwargs.get("session_id") or "",
        "cwd": _cwd(),
        "prompt": kwargs.get("user_message") or "",
        "model": kwargs.get("model") or "",
        "transcript_path": _usage_path(kwargs.get("session_id") or ""),
    }
    try:
        done = subprocess.run(
            [program, command, "--harness", "hermes"], input=json.dumps(payload, default=str),
            capture_output=True, text=True, timeout=timeout,
            env=dict(os.environ, CORK_MACHINE="1", COLLIDE_MACHINE="1"),
        )
        out = (done.stdout or "").strip()
        return json.loads(out) if out.startswith("{") else {}
    except Exception:
        return {}


def _listen(ctx, session_id):
    """One `cork listen` per session: each line it prints is a batch of
    teammates' messages, injected into this conversation."""
    program = _program()
    if not program or not session_id:
        return
    with _lock:
        if session_id in _listeners and _listeners[session_id].poll() is None:
            return
        try:
            child = subprocess.Popen(
                [program, "listen", session_id, "--print"], cwd=_cwd(), stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            )
        except Exception:
            return
        _listeners[session_id] = child

    def pump():
        for line in child.stdout:
            try:
                text = json.loads(line).get("text", "")
            except Exception:
                continue
            if text:
                ctx.inject_message(text, role="user", session_key=session_id)

    threading.Thread(target=pump, daemon=True, name=f"cork-listen-{session_id[:8]}").start()


def register(ctx):
    def pre_llm_call(session_id="", is_first_turn=False, **kwargs):
        kwargs["session_id"] = session_id
        said = []
        with _lock:
            first = session_id not in _briefed
            _briefed.add(session_id)
        if first:
            said.append(_call("report", "on_session_start", kwargs).get("context", ""))
            _listen(ctx, session_id)
        said.append(_call("report", "pre_llm_call", kwargs).get("context", ""))
        said = [s for s in said if s]
        return {"context": "\n\n".join(said)} if said else None

    def pre_tool_call(**kwargs):
        answer = _call("gate", "pre_tool_call", kwargs, timeout=10)
        if answer.get("action") == "block":
            return {"action": "block", "message": answer.get("message", "")}
        return None

    def post_tool_call(**kwargs):
        threading.Thread(target=_call, args=("report", "post_tool_call", kwargs), daemon=True).start()

    def on_session_end(**kwargs):
        threading.Thread(target=_call, args=("report", "on_session_end", kwargs), daemon=True).start()

    def on_session_finalize(session_id="", **kwargs):
        with _lock:
            child = _listeners.pop(session_id, None)
        if child is not None and child.poll() is None:
            child.terminate()

    ctx.register_hook("pre_llm_call", pre_llm_call)
    ctx.register_hook("pre_tool_call", pre_tool_call)
    ctx.register_hook("post_tool_call", post_tool_call)
    ctx.register_hook("on_session_end", on_session_end)
    ctx.register_hook("on_session_finalize", on_session_finalize)
    # what each model call spent, billed at the turn's end (on_session_end)
    ctx.register_hook("post_api_request", _record_usage)
