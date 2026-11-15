#!/usr/bin/env python3
"""Summarize actual mixed-load JSON reports, never synthesize results."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('reports', nargs='+', type=Path)
args = parser.parse_args()
for path in args.reports:
    d = json.loads(path.read_text())
    print(f'\n{path.name}: success={d["success"]} elapsed={d["elapsed_seconds"]:.3f}s')
    print('streams', json.dumps(d['streams']))
    print('purchases', json.dumps(d['purchases']))
    print('ledger', json.dumps(d['ledger']))
    print('heartbeat', json.dumps(d['heartbeat']))
    print('| Endpoint | OK | Fallos | p50 ms | p99 ms |')
    print('|---|---:|---:|---:|---:|')
    for e in d['endpoints']:
        print(f'| {e["endpoint"]} | {e["ok"]} | {e["failed"]} | {(e["p50_us"] or 0)/1000:.2f} | {(e["p99_us"] or 0)/1000:.2f} |')
