"""Local protocol fixture: persists prompts and refuses nonexistent sessions.

No model service or installed coding agent is used by the restoration tests.
"""
import json
import os
from pathlib import Path
import sys

protocol, directory = sys.argv[1:3]
directory = Path(directory)
session_file = directory / "conversation.json"
requests_file = directory / "requests.jsonl"
session_id = "conversation-1"


def send(value):
    print(json.dumps(value), flush=True)


def reply(request, result=None, error=None):
    if protocol == "claude":
        response = {"subtype": "success", "request_id": request["request_id"], "response": result}
        if error:
            response = {"subtype": "error", "request_id": request["request_id"], "error": error}
        send({"type": "control_response", "response": response})
    elif protocol == "pi":
        send({"type": "response", "id": request["id"], "success": not error,
              "data": result, "error": error})
    else:
        response = {"jsonrpc": "2.0", "id": request["id"]}
        response.update({"error": {"code": -32602, "message": error}} if error else {"result": result})
        send(response)


def update(kind, **fields):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session_id, "update": {"sessionUpdate": kind, **fields}}})


def answer(text):
    history = json.loads(session_file.read_text())
    result = history[0] if history else text
    history.append(text)
    session_file.write_text(json.dumps(history))
    return result


for line in sys.stdin:
    request = json.loads(line)
    with requests_file.open("a") as log:
        log.write(json.dumps(request) + "\n")
    method = request.get("method", request.get("type"))
    params = request.get("params", {})
    if method == "initialized":
        continue
    if method == "control_request":
        if "--resume" in sys.argv:
            cursor = sys.argv[sys.argv.index("--resume") + 1]
            if cursor != session_id or not session_file.is_file():
                reply(request, error="session not found")
                continue
        else:
            session_file.write_text("[]")
        reply(request, {})
    elif method == "initialize":
        reply(request, {"protocolVersion": 1, "agentCapabilities": {
            "loadSession": os.environ.get("KUMU_FAKE_NO_LOAD") != "1"}} if protocol == "acp" else {})
    elif method == "account/read":
        reply(request, {"requiresOpenaiAuth": False})
    elif method in ("session/new", "thread/start"):
        session_file.write_text("[]")
        reply(request, {"sessionId": session_id} if protocol == "acp" else {"thread": {"id": session_id}})
    elif method in ("session/load", "thread/resume", "switch_session"):
        cursor = params.get("sessionId", params.get("threadId", request.get("sessionPath")))
        expected = str(session_file) if protocol == "pi" else session_id
        if cursor != expected or not session_file.is_file():
            reply(request, error="session not found")
        else:
            if protocol == "acp":
                update("agent_message_chunk", content={"type": "text", "text": "REPLAY"})
                update("agent_thought_chunk", content={"type": "text", "text": "OLD THOUGHT"})
                update("tool_call", toolCallId="old", title="old tool", status="completed")
            reply(request, {"thread": {"id": session_id}} if protocol == "codex" else {})
    elif method == "get_state":
        if not session_file.is_file():
            session_file.write_text("[]")
        reply(request, {"sessionFile": str(session_file), "sessionId": session_id})
    elif method in ("model/list", "skills/list"):
        reply(request, {"data": []})
    elif method.startswith("get_"):
        reply(request, [])
    elif method in ("session/prompt", "turn/start", "prompt", "user"):
        if protocol == "acp":
            result = answer(params["prompt"][0]["text"])
            update("agent_message_chunk", content={"type": "text", "text": result})
            reply(request, {"stopReason": "end_turn"})
        elif protocol == "codex":
            result = answer(params["input"][0]["text"])
            reply(request, {"turn": {"id": "turn-1"}})
            send({"method": "item/agentMessage/delta", "params": {"itemId": "reply", "delta": result}})
            send({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif protocol == "pi":
            result = answer(request["message"])
            reply(request, {})
            send({"type": "agent_start"})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": result}})
            send({"type": "agent_settled"})
        else:
            result = answer(request["message"]["content"])
            send({"type": "system", "subtype": "init", "session_id": session_id})
            send({"type": "assistant", "message": {"id": "reply", "content": [{"type": "text", "text": result}]}})
            send({"type": "result", "subtype": "success"})
    else:
        reply(request, error="unsupported request")
