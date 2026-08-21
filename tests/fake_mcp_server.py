#!/usr/bin/env python3
"""Minimal MCP streamable-HTTP server for fxrs integration testing."""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18765

TOOLS = [
    {
        "name": "echo",
        "description": "Echo a message back",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "Add two integers",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"},
            },
            "required": ["a", "b"],
        },
    },
]


class Handler(BaseHTTPRequestHandler):
    server_version = "fxrs-test-mcp/1.0"

    def log_message(self, fmt, *args):
        sys.stderr.write("server: " + fmt % args + "\n")

    def _sse(self, events):
        body = "".join(f"event: {k}\ndata: {v}\n\n" for k, v in events)
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Mcp-Protocol-Version", "2025-11-25")
        self.send_header("Mcp-Session-Id", "sess-test-1")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            req = json.loads(raw)
        except Exception:
            self.send_response(400)
            self.end_headers()
            return
        mid = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}

        def rpc_result(result):
            return {"jsonrpc": "2.0", "id": mid, "result": result}

        if mid is None:
            # notification
            self.send_response(202)
            self.end_headers()
            return

        if method == "initialize":
            self._sse(
                [
                    ("mcp-session-id", "sess-test-1"),
                    (
                        "message",
                        json.dumps(
                            rpc_result(
                                {
                                    "protocolVersion": "2025-11-25",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "fxrs-test", "version": "1.0"},
                                }
                            )
                        ),
                    ),
                ]
            )
        elif method == "tools/list":
            self._sse([("message", json.dumps(rpc_result({"tools": TOOLS})))])
        elif method == "tools/call":
            name = params.get("name")
            args = params.get("arguments") or {}
            if name == "echo":
                result = {"content": [{"type": "text", "text": args.get("text", "")}]}
            elif name == "add":
                result = {
                    "content": [
                        {
                            "type": "text",
                            "text": str(int(args.get("a", 0)) + int(args.get("b", 0))),
                        }
                    ]
                }
            else:
                result = {
                    "content": [{"type": "text", "text": f"unknown tool {name}"}],
                    "isError": True,
                }
            self._sse([("message", json.dumps(rpc_result(result)))])
        else:
            self._sse(
                [
                    (
                        "message",
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "id": mid,
                                "error": {"code": -32601, "message": f"method not found: {method}"},
                            }
                        ),
                    )
                ]
            )


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"listening on {PORT}", flush=True)
    srv.serve_forever()
