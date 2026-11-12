"""A Lambda function URL in front of the AWS Lambda Runtime Interface Emulator, for the
rehearsal's function test (tests/search/aws/localstack.sh).

Each HTTP request becomes a function URL event (payload format 2.0: rawPath, rawQueryString,
lower-case headers joined with commas, cookies, requestContext.http, the body base64-encoded
when it isn't text) posted to the emulator's invoke endpoint; the function's answer (statusCode,
headers, cookies, body, isBase64Encoded) becomes the HTTP response, as a BUFFERED function URL
does. Standard library only.

Usage: python3 function_url.py <listen port> <emulator invoke URL>
"""

import base64
import json
import sys
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

INVOKE = ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def handle_any(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0) or 0))
        path, _, query = self.path.partition("?")
        headers = {}
        cookies = []
        for k, v in self.headers.items():
            if k.lower() == "cookie":
                cookies.extend(c.strip() for c in v.split(";"))
                continue
            headers[k.lower()] = f"{headers[k.lower()]},{v}" if k.lower() in headers else v
        try:
            text = body.decode("utf-8")
            encoded = False
        except UnicodeDecodeError:
            text = base64.b64encode(body).decode()
            encoded = True
        now = time.time()
        event = {
            "version": "2.0",
            "routeKey": "$default",
            "rawPath": path,
            "rawQueryString": query,
            "headers": headers,
            "requestContext": {
                "accountId": "anonymous",
                "apiId": "rehearsal",
                "domainName": headers.get("host", ""),
                "http": {"method": self.command, "path": path, "protocol": "HTTP/1.1", "sourceIp": self.client_address[0], "userAgent": headers.get("user-agent", "")},
                "requestId": f"rehearsal-{now}",
                "routeKey": "$default",
                "stage": "$default",
                "time": time.strftime("%d/%b/%Y:%H:%M:%S +0000", time.gmtime(now)),
                "timeEpoch": int(now * 1000),
            },
            "isBase64Encoded": encoded,
        }
        if cookies:
            event["cookies"] = cookies
        if body:
            event["body"] = text
        req = urllib.request.Request(INVOKE, data=json.dumps(event).encode(), method="POST")
        with urllib.request.urlopen(req, timeout=180) as r:
            out = json.loads(r.read())
        status = int(out.get("statusCode", 200))
        data = out.get("body", "") or ""
        data = base64.b64decode(data) if out.get("isBase64Encoded") else data.encode()
        self.send_response(status)
        for k, v in (out.get("headers") or {}).items():
            if k.lower() not in ("content-length", "transfer-encoding", "connection"):
                self.send_header(k, v)
        for c in out.get("cookies") or []:
            self.send_header("set-cookie", c)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = do_PATCH = handle_any


if __name__ == "__main__":
    INVOKE = sys.argv[2]
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1])), Handler).serve_forever()
