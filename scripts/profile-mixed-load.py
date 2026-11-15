#!/usr/bin/env python3
"""Profile the process launched for an isolated benchmark, never a live server.

Measurements exclude child processes. The mixed in-process harness includes
proxy/client/mock; multiprocess harnesses must use their own per-role reports.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import signal
import time


def snapshot(pid):
    root = Path('/proc') / str(pid)
    try:
        status = {}
        for line in (root / 'status').read_text().splitlines():
            if ':' in line:
                key, value = line.split(':', 1)
                status[key] = value.strip()
        raw = (root / 'stat').read_text().rsplit(')', 1)[1].split()
        io = {}
        for line in (root / 'io').read_text().splitlines():
            key, value = line.split(':', 1)
            io[key] = int(value)
        return {
            'monotonic': time.monotonic(),
            'rss_kib': int(status.get('VmRSS', '0 kB').split()[0]),
            'threads': int(status.get('Threads', '0')),
            'fds': len(list((root / 'fd').iterdir())),
            'user_ticks': int(raw[11]),
            'system_ticks': int(raw[12]),
            'minor_faults': int(raw[7]),
            'major_faults': int(raw[9]),
            'io': io,
        }
    except (OSError, ValueError, IndexError):
        return None


def compiler_pids():
    found = []
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            name = (proc / 'comm').read_text().strip()
            if name in {'cargo', 'rustc', 'rust-lld', 'cc1', 'cc1plus'}:
                found.append((int(proc.name), name))
        except OSError:
            pass
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command and command[0] == '--':
        command = command[1:]
    if not command:
        parser.error('missing command')
    if compiler_pids():
        raise SystemExit(f'build concurrente, timings inválidos: {compiler_pids()}')
    args.output.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with (args.output / 'stdout.log').open('w') as stdout, (args.output / 'stderr.log').open('w') as stderr:
        child = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=os.environ.copy(), start_new_session=True)
        samples = []
        timed_out = False
        compilers_during = []
        next_compiler_check = started + 1
        while child.poll() is None:
            now = time.monotonic()
            if now - started > args.timeout:
                timed_out = True
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                break
            if now >= next_compiler_check:
                compilers_during.extend(compiler_pids())
                next_compiler_check = now + 1
            sample = snapshot(child.pid)
            if sample:
                samples.append(sample)
            time.sleep(0.025)
        code = child.wait()
    compilers_after = compiler_pids()
    result = {
        'command': command,
        'pid': child.pid,
        'exit_code': code,
        'scope': 'launched process only, excluding children; includes setup and drain',
        'duration_seconds': time.monotonic() - started,
        'sample_interval_seconds': 0.025,
        'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
        'timed_out': timed_out,
        'timings_valid_no_builds': not (compilers_after or compilers_during),
        'compilers_during': sorted(set(compilers_during)),
        'compilers_after': compilers_after,
        'peak_rss_mib': max((s['rss_kib'] for s in samples), default=0) / 1024,
        'peak_threads': max((s['threads'] for s in samples), default=0),
        'peak_fds': max((s['fds'] for s in samples), default=0),
        'samples': samples,
    }
    if samples:
        last = samples[-1]
        result['sampled_cpu_seconds'] = (last['user_ticks'] + last['system_ticks']) / os.sysconf('SC_CLK_TCK')
        result['sampled_storage_written_bytes'] = last['io'].get('write_bytes')
    (args.output / 'resources.json').write_text(json.dumps(result, indent=2))
    print(json.dumps({k: v for k, v in result.items() if k != 'samples'}, indent=2))
    raise SystemExit(code or (2 if compilers_after or compilers_during or timed_out else 0))


if __name__ == '__main__':
    main()
