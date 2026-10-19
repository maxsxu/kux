#!/usr/bin/env python3
"""Replay merged logs using the isolated sibling PTY harness.

Run after cargo build: python3 scripts/smoke_merged_logs.py
KUX_BINARY may override target/debug/kux. Python's standard library is sufficient.
Only generated demo resources are used; a failing kubectl trap records any accidental
subprocess invocation. Dumps, screen captures and a JSON report remain in system temp.
"""
import datetime
import hashlib
import importlib.util
import json
import pathlib
import re
import sys
import time

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    'references_smoke', pathlib.Path(__file__).resolve().with_name('smoke_usedby.py')
)
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)
OUT = harness.OUT
DUMPS = OUT / 'dumps'
Session = harness.Session
check = harness.check


def wait_logs(session):
    return session.wait(
        lambda text: 'log ' in text and 'complete' in text,
        'completed demo logs', timeout=5,
    )


def save_logs(session, label):
    before = set(DUMPS.rglob('*.log'))
    session.send(b'\x13', 0.2)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        created = set(DUMPS.rglob('*.log')) - before
        if created:
            check(len(created) == 1, 'one dump per save')
            path = created.pop()
            text = path.read_text()
            check('\x1b' not in text, 'exports contain no terminal styling')
            session.save(label)
            return text
        session.read(0.05)
    raise AssertionError('log export did not complete: ' + label)


def pod_all_containers(session):
    session.send('l', 0.3)
    wait_logs(session)
    check('<a>' in session.screen.text(), 'default container exposes all-container toggle')
    default = save_logs(session, 'pod-default')
    check(default.startswith('app '), 'default Pod source prefix')
    check('sidecar ' not in default, 'default Pod scope excludes other containers')
    session.send('a', 0.3)
    wait_logs(session)
    merged = save_logs(session, 'pod-all')
    check(any(line.startswith('app ') for line in merged.splitlines()), 'all includes app')
    check(any(line.startswith('sidecar ') for line in merged.splitlines()), 'all includes sidecar')
    session.send('t')
    session.send('/sidecar\r')
    filtered = save_logs(session, 'pod-filtered-timestamped')
    check(len(filtered.splitlines()) == 100, 'filtered export retains complete matching buffer')
    check(all(re.match(r'^2026-09-07T\d\d:\d\d:\d\dZ +sidecar ', line) for line in filtered.splitlines()), 'timestamp is before source on every filtered line')
    session.send('2', 0.3)
    wait_logs(session)
    ranged = save_logs(session, 'pod-reloaded-range')
    check(len(ranged.splitlines()) == 6, 'one-minute reload applies per source and keeps filter')
    check(all(' sidecar ' in line for line in ranged.splitlines()), 'range preserves all-container state')
    session.send('\x1b')
    session.send('m')
    marked = save_logs(session, 'pod-marker')
    check(any(line.startswith('────') for line in marked.splitlines()), 'local marker survives export')
    session.send('a', 0.3)
    wait_logs(session)
    single = save_logs(session, 'pod-default-restored')
    check('sidecar ' not in single and ' app ' in single, 'toggle restores default container')
    session.send('\x1b')
    check('pods(all)' in session.screen.text(), 'Escape returns to Pod list')


def workload_routes(session):
    for resource, prefix in [
        ('deployments', 'deploy'), ('statefulsets', 'sts'),
        ('daemonsets', 'ds'), ('services', 'svc'), ('jobs', 'jobs'),
    ]:
        session.send(':' + resource + '\r', 0.3)
        for key, mode in [('l', 'current'), ('p', 'previous')]:
            session.send(key, 0.3)
            wait_logs(session)
            text = save_logs(session, resource + '-' + mode)
            names = [prefix + '-0000000-pod-' + str(index) for index in (0, 1)]
            for name in names:
                check(any(line.startswith(name + ' ') for line in text.splitlines()), 'workload source prefix ' + name)
            check(mode + ' log ' in text, resource + ' ' + mode + ' route')
            if resource == 'services':
                check('<a>' not in session.screen.text(), 'Service without default has no toggle')
                check(' sidecar ' in text, 'Service opens every container')
            else:
                check('<a>' in session.screen.text(), 'workload default has toggle')
                check(' sidecar ' not in text, 'workload starts at annotated default')
            session.send('\x1b')


def explicit_container(session):
    session.send('\r', 0.3)
    session.wait(lambda text: 'sidecar' in text and 'containers(' in text, 'Pod container list')
    session.send('/sidecar\r')
    session.send('\r', 0.3)
    wait_logs(session)
    check('<a>' not in session.screen.text(), 'explicit container has no all-container toggle')
    before = save_logs(session, 'explicit-sidecar')
    check('sidecar current log ' in before, 'explicit sidecar content')
    check(not any(line.startswith('sidecar ') for line in before.splitlines()), 'single-container prefix is omitted')
    session.send('a')
    after = save_logs(session, 'explicit-sidecar-unchanged')
    check(before == after, 'all-container key cannot widen explicit scope')


def readonly_merged(session):
    session.send(':deployments\r', 0.3)
    session.send('l', 0.3)
    wait_logs(session)
    session.send('a', 0.3)
    wait_logs(session)
    text = save_logs(session, 'readonly-merged')
    check('deploy-0000000-pod-0 app ' in text, 'read-only first Pod app')
    check('deploy-0000000-pod-1 sidecar ' in text, 'read-only second Pod sidecar')


def main():
    for directory in ['bin', 'config', 'data', 'dumps']:
        (OUT / directory).mkdir()
    trap = OUT / 'bin/kubectl'
    trap.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$KUX_PTY_TRAP"\nexit 91\n')
    trap.chmod(0o755)
    report = {
        'created_at': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
        'binary': str(harness.BINARY),
        'binary_sha256': hashlib.sha256(harness.BINARY.read_bytes()).hexdigest(),
        'groups': [], 'failures': [],
    }
    for name, extra, work in [
        ('pod-all-filter-range-export', (), pod_all_containers),
        ('workload-current-and-previous', (), workload_routes),
        ('explicit-container', (), explicit_container),
        ('readonly-merged', ('--read-only',), readonly_merged),
    ]:
        session = Session(name, ('--screen-dump-dir', str(DUMPS), *extra))
        try:
            work(session)
            report['groups'].append({'name': name, 'passed': True})
        except Exception as error:
            session.save('failure')
            report['groups'].append({'name': name, 'passed': False})
            report['failures'].append({'name': name, 'error': str(error)})
        finally:
            session.close()
    report['kubectl_called'] = (OUT / 'kubectl.log').exists()
    if report['kubectl_called']:
        report['failures'].append({'name': 'kubectl', 'error': 'unexpected invocation'})
    report['dump_count'] = len(list(DUMPS.rglob('*.log')))
    path = OUT / 'merged-logs-report.json'
    path.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'report': str(path), **report}, indent=2))
    return bool(report['failures'])


if __name__ == '__main__':
    raise SystemExit(main())
