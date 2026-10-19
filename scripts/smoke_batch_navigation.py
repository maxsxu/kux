#!/usr/bin/env python3
"""Replay CronJob/Job navigation using the isolated sibling PTY harness.

Run after cargo build: python3 scripts/smoke_batch_navigation.py
KUX_BINARY may override the default target/debug/kux binary. The harness uses only
Python standard library modules, demo resources and a failing kubectl trap. It
prints the retained JSON report and rendered-screen directory.
"""
import datetime
import hashlib
import importlib.util
import json
import pathlib
import sys

sys.dont_write_bytecode = True

spec=importlib.util.spec_from_file_location('references_smoke',pathlib.Path(__file__).resolve().with_name('smoke_usedby.py'))
harness=importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)
OUT=harness.OUT
Session=harness.Session
check=harness.check

def route(s):
    s.send(':cronjobs\r',0.3)
    text=s.send('/cronjobs-0000000\r')
    check('Jobs' in text and '<enter>' in text,'CronJob Enter advertises Jobs')
    s.save('cronjob-source')
    text=s.send('\r',0.35)
    s.wait(lambda text:'jobs-0000000' in text and 'Pods' in text,'CronJob Jobs load')
    text=s.screen.text()
    check('jobs-0000250' not in text,'CronJob owner filter excludes same-namespace unrelated Job')
    check('team-000' in text,'CronJob Jobs selected namespace')
    s.save('owned-jobs')
    text=s.send('\r',0.35)
    s.wait(lambda text:'po-0000000' in text and 'Containers' in text,'Job Pods load')
    text=s.screen.text()
    check('team-000' in text and 'po-0000001' not in text,'Job Pods selected namespace')
    s.save('job-pods')
    text=s.send('\x1b',0.3)
    check('jobs-0000000' in text and 'Pods' in text,'Escape restores Jobs')
    check('jobs-0000250' not in text,'Escape restores owner filter')
    s.save('returned-jobs')
    text=s.send('\x1b',0.3)
    check('cronjobs-0000000' in text and 'Jobs' in text,'Escape restores CronJobs')
    s.save('returned-cronjobs')

def direct(s):
    s.send(':jobs\r',0.3)
    text=s.send('/jobs-0000001\r')
    check('Pods' in text and '<enter>' in text,'Job Enter advertises Pods')
    text=s.send('\r',0.35)
    s.wait(lambda text:'po-0000001' in text and 'Containers' in text,'direct Job Pods load')
    text=s.screen.text()
    check('po-0000000' not in text and 'team-001' in text,'direct Job scopes selected namespace')
    s.save('direct-job-pods')
    text=s.send('\x1b',0.3)
    check('jobs-0000001' in text and 'Pods' in text,'direct Job return')
    s.save('direct-job-return')

def main():
    (OUT/'bin').mkdir();(OUT/'config').mkdir();(OUT/'data').mkdir()
    trap=OUT/'bin/kubectl'
    trap.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$KUX_PTY_TRAP"\nexit 91\n')
    trap.chmod(0o755)
    report={'created_at':datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),'binary':str(harness.BINARY),'binary_sha256':hashlib.sha256(harness.BINARY.read_bytes()).hexdigest(),'groups':[],'failures':[]}
    for name,extra,work in [('cronjob-jobs-pods',(),route),('readonly-cronjob-jobs-pods',('--read-only',),route),('direct-job-pods',(),direct)]:
        s=Session(name,extra)
        try: work(s);report['groups'].append({'name':name,'passed':True})
        except Exception as exc:
            s.save('failure');report['failures'].append({'name':name,'error':str(exc)});report['groups'].append({'name':name,'passed':False})
        finally:s.close()
    report['kubectl_called']=(OUT/'kubectl.log').exists()
    if report['kubectl_called']:report['failures'].append({'name':'kubectl','error':'unexpected kubectl invocation'})
    (OUT/'batch-report.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'report':str(OUT/'batch-report.json'),'groups':report['groups'],'failures':report['failures'],'kubectl_called':report['kubectl_called']},indent=2))
    return bool(report['failures'])

if __name__=='__main__':raise SystemExit(main())
