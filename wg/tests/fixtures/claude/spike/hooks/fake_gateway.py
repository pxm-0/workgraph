# Loopback stand-in for an Anthropic-compatible gateway: logs each request's path and model, replies 404.
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
LOG = sys.argv[2]
class H(BaseHTTPRequestHandler):
    def _h(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n) if n else b""
        try: model = json.loads(body).get("model")
        except Exception: model = None
        key = self.headers.get("x-api-key") or self.headers.get("authorization") or ""
        with open(LOG, "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "model": model,
                                "auth_header": "x-api-key" if self.headers.get("x-api-key") else ("authorization" if key else None),
                                "dummy_key_seen": "SPIKE-DUMMY" in key}) + "\n")
        out = json.dumps({"type": "error", "error": {"type": "not_found_error", "message": "spike gateway: no upstream (dry run)"}}).encode()
        self.send_response(404); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out)
    do_GET = do_POST = do_HEAD = _h
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
