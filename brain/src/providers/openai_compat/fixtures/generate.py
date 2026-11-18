"""Writes the OpenAI-compatible SSE test fixtures from the documented chunk shape
(OpenAI spec: CreateChatCompletionStreamResponse, ChatCompletionMessageToolCallChunk; usage in
the last chunk with stream_options.include_usage). Hand-built, not captured from live traffic.
Run: python3 -I generate.py <this directory>"""
import json, os, sys

out = sys.argv[1]


def chunk(delta=None, finish=None, usage=None, choices=True):
    body = {"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "qwen3-8b"}
    body["choices"] = [{"index": 0, "delta": delta or {}, "finish_reason": finish}] if choices else []
    if usage is not None:
        body["usage"] = usage
    return f"data: {json.dumps(body, separators=(',', ':'))}\n\n"


ROLE = chunk({"role": "assistant", "content": ""})
DONE = "data: [DONE]\n\n"
USAGE = chunk(choices=False, usage={"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150})


def text(*pieces):
    return "".join(chunk({"content": p}) for p in pieces)


def call(index, id=None, name=None, arguments=None):
    piece = {"index": index}
    if id is not None:
        piece["id"] = id
        piece["type"] = "function"
    function = {}
    if name is not None:
        function["name"] = name
    if arguments is not None:
        function["arguments"] = arguments
    if function:
        piece["function"] = function
    return chunk({"tool_calls": [piece]})


fixtures = {
    "text.sse": ROLE + text("Hello", " there.") + chunk(finish="stop") + USAGE + DONE,
    "tool_call.sse": ROLE + text("Let me look.") + call(0, "call_1", "exec", "")
        + call(0, arguments='{"command": "ls') + call(0, arguments=' -la"}') + chunk(finish="tool_calls") + USAGE + DONE,
    "parallel_tool_calls.sse": ROLE + call(0, "call_1", "exec", "") + call(1, "call_2", "notes__search", "")
        + call(0, arguments='{"command":"cargo test"}') + call(1, arguments='{"query":"build notes"}')
        + chunk(finish="tool_calls") + USAGE + DONE,
    # Ollama style: the whole call in one chunk, finish "stop", no [DONE] needed after a finish.
    "whole_call_stop.sse": ROLE + chunk({"tool_calls": [{"index": 0, "id": "call_9", "type": "function",
        "function": {"name": "exec", "arguments": '{"command":"pwd"}'}}]}) + chunk(finish="stop") + USAGE,
    "length.sse": ROLE + text("Partial an") + call(0, "call_3", "exec", '{"comm') + chunk(finish="length") + USAGE + DONE,
    "content_filter.sse": ROLE + text("I will") + chunk(finish="content_filter") + USAGE + DONE,
    "stream_error.sse": ROLE + text("Working") + 'data: {"error":{"message":"model unloaded","type":"server_error"}}\n\n',
    "malformed.sse": ROLE + 'data: {"choices":[{"delta":\n\n',
    "truncated.sse": ROLE + text("Cut off"),
    "unknown_tool.sse": ROLE + call(0, "call_4", "rm_everything", "{}") + chunk(finish="tool_calls") + DONE,
    "invalid_arguments.sse": ROLE + call(0, "call_5", "exec", '{"command": 5}') + chunk(finish="tool_calls") + DONE,
    # OpenRouter reports the step's cost (credits = US dollars) in the usage chunk.
    "openrouter_text.sse": ROLE + text("Hi from OpenRouter.") + chunk(finish="stop")
        + chunk(choices=False, usage={"prompt_tokens": 50, "completion_tokens": 5, "total_tokens": 55, "cost": 0.000421})
        + DONE,
    "no_ids.sse": ROLE + call(0, None, "exec", '{"command":"ls"}') + call(1, None, "exec", '{"command":"pwd"}')
        + chunk(finish="tool_calls") + DONE,
}
for name, body in fixtures.items():
    with open(os.path.join(out, name), "w") as f:
        f.write(body)

models = {
    "models_local.json": {"object": "list", "data": [
        {"id": "qwen3-8b", "object": "model", "owned_by": "organization_owner"},
        {"id": "llama-3.2-3b-instruct", "object": "model", "owned_by": "organization_owner"}]},
    # The richer shape some hosted servers return (supported_parameters, pricing as strings).
    "models_rich.json": {"data": [
        {"id": "vendor/tool-model", "supported_parameters": ["tools", "temperature"], "pricing": {"prompt": "0.000001", "completion": "0.000002"}},
        {"id": "vendor/free-model:free", "supported_parameters": ["temperature"], "pricing": {"prompt": "0", "completion": "0"}},
        {"id": "vendor/zero-priced", "capabilities": ["tool_use"], "pricing": {"prompt": "0", "completion": "0"}}]},
}
for name, body in models.items():
    with open(os.path.join(out, name), "w") as f:
        json.dump(body, f, indent=2)
print(len(fixtures) + len(models), "fixtures")
