#!/usr/bin/env python3
"""A search backend that follows the `memory` contract (docs/memory.md "Search backend"), for
scripts/test-memory.py. It answers `search`, `reindex` and `status` and keeps no store.

  FAKE_MEMORY_HITS  a JSON file: the hits `search` returns, whatever the query
  FAKE_MEMORY_MODE  ok (default) | exit3 | exit1 | garbage | hang (sleeps 60 s)
  FAKE_MEMORY_LOG   a file; each call appends its arguments as one JSON line
"""
import json
import os
import sys
import time

args = sys.argv[1:]
log = os.environ.get('FAKE_MEMORY_LOG')
if log:
    with open(log, 'a') as file:
        file.write(json.dumps(args) + '\n')
mode = os.environ.get('FAKE_MEMORY_MODE', 'ok')
scope = args[args.index('--scope') + 1] if '--scope' in args else ''
if mode == 'exit3':
    print('fake-memory-backend: the index is locked', file=sys.stderr)
    sys.exit(3)
if mode == 'exit1':
    sys.exit(1)
if mode == 'hang':
    time.sleep(60)
if mode == 'garbage':
    print('not json')
    sys.exit(0)
command = args[0] if args else ''
if command == 'search':
    hits = json.load(open(os.environ['FAKE_MEMORY_HITS'])) if os.environ.get('FAKE_MEMORY_HITS') else []
    print(json.dumps(hits))
elif command == 'reindex':
    print(json.dumps({'scope': scope, 'ok': True}))
elif command == 'status':
    print(json.dumps({'scope': scope, 'store': '/fake', 'backend': 'fake', 'search': {'network': True, 'model': False},
                      'counts': {'active': 0, 'superseded': 0, 'failed': 0}}))
else:
    print('fake-memory-backend: unknown command', file=sys.stderr)
    sys.exit(2)
