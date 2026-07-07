"""Fake OpenAI upstream for the serve-proxy e2e: a labeled test fake, never
a mock pretending to be the provider. Answers every POST
/v1/chat/completions with one canned chat-completion body (pinned model id,
fixed usage) so the proxy's recording — relay, ingestion, attrs — can be
asserted without any network beyond loopback and without any paid call."""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

BODY = {
    "id": "chatcmpl-fake-upstream",
    "object": "chat.completion",
    "model": "gpt-5.4-mini",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "fake upstream answer"},
            "finish_reason": "stop",
        }
    ],
    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 (BaseHTTPRequestHandler naming)
        self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.path != "/v1/chat/completions":
            self.send_response(404)
            self.end_headers()
            return
        payload = json.dumps(BODY).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # keep e2e output clean
        pass


if __name__ == "__main__":
    port = int(sys.argv[1])
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
