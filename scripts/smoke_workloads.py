#!/usr/bin/env python3
"""Replay aggregate Workloads and singleton navigation using only generated demo data.

Run after cargo build: python3 scripts/smoke_workloads.py
KUX_BINARY may override target/debug/kux. Uses the sibling standard-library POSIX
PTY harness, isolated config/kubeconfig, and a failing kubectl trap. No deletion
is confirmed. Reports, CSV exports and screen captures remain in system temp.
"""
import csv
import datetime
import hashlib
import importlib.util
import io
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
KINDS = {
    'v1/pods', 'v1/services', 'apps/v1/daemonsets',
    'apps/v1/statefulsets', 'apps/v1/deployments', 'apps/v1/replicasets',
}

def natural_ready(value):
    return tuple((1, int(part)) if part.isdigit() else (0, part)
                 for part in re.split(r'(\d+)', value) if part)


def displayed_age(value):
    if value == '<unknown>':
        return 0
    match = re.fullmatch(r'(\d+)([smhdy])', value)
    check(match is not None, 'recognized displayed age ' + value)
    return int(match[1]) * {'s': 1, 'm': 60, 'h': 3600, 'd': 86400, 'y': 31536000}[match[2]]


def workloads(session, command='wk'):
    session.send(':' + command + '\r', 0.3)
    return session.wait(lambda text: 'workloads(all)[' in text and 'KIND' in text, 'Workloads table')


def export_csv(session, label):
    before = set(DUMPS.rglob('*.csv'))
    session.send(b'\x13', 0.2)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        created = set(DUMPS.rglob('*.csv')) - before
        if created:
            check(len(created) == 1, 'one CSV per save')
            text = created.pop().read_text()
            check('\x1b' not in text, 'CSV has no terminal escape sequences')
            session.save(label)
            return list(csv.reader(io.StringIO(text)))
        session.read(0.05)
    raise AssertionError('CSV export did not complete: ' + label)


def table_sort_export(session):
    workloads(session)
    table = export_csv(session, 'kind-ascending')
    check(table[0] == ['KIND', 'NAMESPACE', 'NAME', 'STATUS', 'READY', 'AGE'], 'pinned normal columns')
    check(len(table) == 1801, 'complete six-kind export beyond viewport')
    check(set(row[0] for row in table[1:]) == KINDS, 'all six workload kinds')
    check([row[0] for row in table[1:]] == sorted(row[0] for row in table[1:]), 'initial Kind ascending')
    session.send('K')
    reverse = export_csv(session, 'kind-descending')
    check([row[0] for row in reverse[1:]] == sorted((row[0] for row in reverse[1:]), reverse=True), 'Kind shortcut reverses order')
    session.send('R')
    ready = export_csv(session, 'ready-ascending')
    check([row[4] for row in ready[1:]] == sorted((row[4] for row in ready[1:]), key=natural_ready), 'Ready shortcut uses natural order')
    session.send('A')
    age = export_csv(session, 'age-youngest-first')
    ages = [displayed_age(row[5]) for row in age[1:]]
    check(ages == sorted(ages), 'Age shortcut shows youngest first')
    session.send(b'\x17')
    wide = export_csv(session, 'wide-validity')
    check(wide[0] == ['KIND', 'NAMESPACE', 'NAME', 'STATUS', 'READY', 'VALID', 'AGE'], 'VALID is wide-only')
    check(all(row[5] == ('DEGRADED' if row[3] == 'DEGRADED' else '') for row in wide[1:]), 'wide validity matches pinned status projection')
    session.send(b'\x1b[1;2D')
    session.send('O')
    validity = export_csv(session, 'selected-validity-sort')
    check([row[5] for row in validity[1:]] == sorted(row[5] for row in validity[1:]), 'visible-column selection sorts VALID before AGE')
    session.send(b'\x1a')
    faults = export_csv(session, 'faults')
    check(len(faults) > 1 and all(row[3] == 'DEGRADED' for row in faults[1:]), 'faults filter follows workload status')


def singleton_and_mixed_marks(session):
    workloads(session, 'workloads')
    session.send('/0000000\r')
    table = export_csv(session, 'one-of-each-kind')
    check(len(table) == 7 and set(row[0] for row in table[1:]) == KINDS, 'filter keeps one of each kind')
    session.send('g j ')
    check(session.screen.text().count('●') == 2, 'two differently typed rows are marked')
    session.send(b'\x04')
    check('Delete all 2 selected resources?' in session.screen.text(), 'mixed delete confirmation uses resource label')
    session.save('mixed-delete-canceled')
    session.send('\x1b')
    session.send('g')
    first = table[1]
    session.send('y', 0.3)
    session.wait(lambda text: 'YAML(' + first[1] + '/' + first[2] + ')' in text, 'mixed-row YAML target')
    session.save('row-yaml')
    session.send('\x1b')
    session.send('\r', 0.3)
    title = first[0].split('/')[-1] + '(' + first[1] + '/' + first[2] + ')[1]'
    session.wait(lambda text: title in text, 'concrete singleton view')
    session.save('singleton')
    session.send('\x1b', 0.3)
    text = session.wait(lambda text: 'workloads(all)[6]' in text and '/0000000' in text, 'aggregate return state')
    check(text.count('●') == 2, 'marks restored after singleton return')
    returned = export_csv(session, 'restored-filter-and-order')
    check([row[:5] for row in returned] == [row[:5] for row in table], 'filter and row order restored')


def readonly_navigation(session):
    workloads(session)
    session.send('/deploy-0000000\r')
    for key in ['l', 'p', 's', 'r', 'i', 'e', b'\x0b', b'\x04']:
        session.send(key)
        text = session.screen.text()
        check('workloads(all)[1]' in text, 'aggregate remains visible after ' + repr(key))
        check('Confirm' not in text and 'Logs(' not in text, 'typed or mutating action is unavailable')
    session.send('\r', 0.3)
    session.wait(lambda text: 'deployments(team-000/deploy-0000000)[1]' in text, 'read-only singleton')
    session.send('y', 0.3)
    session.wait(lambda text: 'YAML(team-000/deploy-0000000)' in text, 'read-only singleton YAML')
    session.save('readonly-yaml')
    session.send('\x1b')
    session.send('\x1b', 0.3)
    check('workloads(all)[1]' in session.screen.text(), 'read-only aggregate return')


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
        ('table-sort-export', (), table_sort_export),
        ('singleton-and-mixed-marks', (), singleton_and_mixed_marks),
        ('readonly-navigation', ('--read-only',), readonly_navigation),
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
    report['csv_count'] = len(list(DUMPS.rglob('*.csv')))
    path = OUT / 'workloads-report.json'
    path.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'report': str(path), **report}, indent=2))
    return bool(report['failures'])


if __name__ == '__main__':
    raise SystemExit(main())
