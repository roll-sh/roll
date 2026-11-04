#!/usr/bin/env python3
"""Bounded diagnostic of the CI Docker-compute to host-gateway HTTP path."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
import subprocess
import threading

IMAGE = 'postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873'
port = 53258
name = f'a162-service-network-{os.getpid()}'
label = f'pg-engine.verification={os.getpid()}'
body = b'pg-engine service network preflight\n'


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


server = HTTPServer(('0.0.0.0', port), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
result = {'bind': '0.0.0.0', 'port': port, 'container': name, 'image': IMAGE,
          'scope': 'diagnostic HTTP path; not proof of historical failure cause'}
try:
    run = subprocess.run([
        'docker', 'run', '--rm', '--name', name,
        '--label', 'agent.worktree=agent-acb216a1743adeadc', '--label', label,
        '--memory', '64m', '--cpu-shares', '2', '--network', 'bridge',
        '--add-host', 'host.docker.internal:host-gateway', '--entrypoint', 'sh', IMAGE,
        '-eu', '-c', f'wget -q -T 5 -O - http://host.docker.internal:{port}/pg-e2e-network-preflight',
    ], capture_output=True, timeout=20)
    result.update(exit=run.returncode, stdout=run.stdout.decode(errors='replace'),
                  stderr=run.stderr.decode(errors='replace'))
    result['status'] = 'PASS' if run.returncode == 0 and run.stdout == body else 'FAIL'
except (OSError, subprocess.TimeoutExpired) as error:
    result.update(status='FAIL', error=str(error))
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)
    # Remove only a container carrying this invocation's exact ownership label.
    owned = subprocess.run(['docker', 'ps', '-aq', '--filter', f'name=^/{name}$',
                            '--filter', f'label={label}'], capture_output=True, text=True)
    for container in owned.stdout.split():
        subprocess.run(['docker', 'rm', '-f', container], check=True, capture_output=True)
print(json.dumps(result, indent=2))
raise SystemExit(result['status'] != 'PASS')
