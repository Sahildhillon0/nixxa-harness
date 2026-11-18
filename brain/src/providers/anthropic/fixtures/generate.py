"""Writes the Anthropic SSE test fixtures from the documented event shapes
(https://platform.claude.com/docs/en/build-with-claude/streaming). Hand-built, not captured
from live traffic. Run: python3 -I generate.py <this directory>"""
import json, sys, os

out = sys.argv[1]

def ev(name, data):
    return f"event: {name}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n"

START = ev("message_start", {"type": "message_start", "message": {
    "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5",
    "content": [], "stop_reason": None, "stop_sequence": None,
    "usage": {"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})
STOP = ev("message_stop", {"type": "message_stop"})
PING = "event: ping\ndata: {\"type\": \"ping\"}\n\n"

def start(i, block): return ev("content_block_start", {"type": "content_block_start", "index": i, "content_block": block})
def delta(i, d): return ev("content_block_delta", {"type": "content_block_delta", "index": i, "delta": d})
def stop(i): return ev("content_block_stop", {"type": "content_block_stop", "index": i})

def thinking(i, sig):
    return (start(i, {"type": "thinking", "thinking": "", "signature": ""})
            + delta(i, {"type": "thinking_delta", "thinking": ""})
            + delta(i, {"type": "signature_delta", "signature": sig}) + stop(i))

def text(i, *pieces):
    return start(i, {"type": "text", "text": ""}) + "".join(delta(i, {"type": "text_delta", "text": p}) for p in pieces) + stop(i)

def tool(i, id, name, *json_pieces):
    return (start(i, {"type": "tool_use", "id": id, "name": name, "input": {}})
            + "".join(delta(i, {"type": "input_json_delta", "partial_json": p}) for p in json_pieces) + stop(i))

def msg_delta(reason, output, **extra):
    d = {"stop_reason": reason, "stop_sequence": None}
    d.update(extra.pop("delta", {}))
    usage = {"output_tokens": output}
    usage.update(extra.pop("usage", {}))
    return ev("message_delta", {"type": "message_delta", "delta": d, "usage": usage})

fixtures = {
    "text.sse": START + PING + thinking(0, "EqQBsig-text") + text(1, "Hello", " there.") + msg_delta("end_turn", 12) + STOP,
    "tool_use.sse": START + thinking(0, "EqQBsig-tool") + text(1, "Let me look.")
        + tool(2, "toolu_01A", "exec", "", '{"command": "ls', ' -la"}') + msg_delta("tool_use", 40) + STOP,
    "parallel_tool_use.sse": START + tool(0, "toolu_01A", "exec", '{"command":"cargo test"}')
        + tool(1, "toolu_01B", "notes__search", '{"query":', '"build notes"}') + msg_delta("tool_use", 50) + STOP,
    "max_tokens.sse": START + text(0, "Partial an") + tool(1, "toolu_01C", "exec", '{"comm') + msg_delta("max_tokens", 64) + STOP,
    "refusal.sse": START + text(0, "I will")
        + msg_delta("refusal", 3, delta={"stop_details": {"type": "refusal", "category": "cyber", "explanation": None}}) + STOP,
    "fallback.sse": START + text(0, "Sure, ") + thinking(1, "declined-sig")
        + tool(2, "toolu_01D", "exec", '{"command":"rm -rf /"}')
        + start(3, {"type": "fallback", "from": {"model": "claude-sonnet-5-5"}, "to": {"model": "claude-sonnet-5"}}) + stop(3)
        + thinking(4, "fallback-sig") + text(5, "Here is the answer.")
        + msg_delta("end_turn", 30, usage={"iterations": [{"type": "message", "output_tokens": 10}, {"type": "fallback_message", "output_tokens": 20}]})
        + STOP,
    "redacted_thinking.sse": START + start(0, {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"}) + stop(0)
        + text(1, "Done thinking.") + msg_delta("end_turn", 9) + STOP,
    "stream_error.sse": START + text(0, "Working").replace(stop(0), "")
        + ev("error", {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    "malformed.sse": START + 'event: content_block_start\ndata: {"type":"content_block_start","index":0,\n\n',
    "truncated.sse": START + text(0, "Cut off"),
    "unknown_tool.sse": START + tool(0, "toolu_01E", "rm_everything", "{}") + msg_delta("tool_use", 5) + STOP,
    "invalid_input.sse": START + tool(0, "toolu_01F", "exec", '{"command": 5}') + msg_delta("tool_use", 5) + STOP,
}
for name, body in fixtures.items():
    with open(os.path.join(out, name), "w") as f:
        f.write(body)
print(len(fixtures), "fixtures")
