#!/usr/bin/env python3
"""Offline control for storage_linked_gc's compression-policy assertions."""
import json
import sys

data = json.load(open(sys.argv[1]))
policy = data['store_compress']
blocks = data['files_compressed']
accepted = bool(blocks) and all(not block or policy for block in blocks) and (any(blocks) == policy)
print(json.dumps({'input': data, 'accepted': accepted}))
raise SystemExit(0 if accepted else 1)
