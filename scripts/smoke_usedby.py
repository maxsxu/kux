#!/usr/bin/env python3
"""Replay UsedBy against generated demo data in an isolated POSIX PTY.

Run after cargo build: python3 scripts/smoke_usedby.py
KUX_BINARY may override the default target/debug/kux binary. Only Python standard
library modules are used. JSON reports, rendered screens and ANSI transcripts are
retained in the printed system-temporary directory. A failing kubectl trap proves
that these controls never invoke kubectl. No Kubernetes credentials are loaded.
"""
import codecs
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import re
import select
import signal
import struct
import tempfile
import termios
import time
import unicodedata

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = pathlib.Path(tempfile.mkdtemp(prefix='kux-references-smoke-'))
BINARY = pathlib.Path(os.environ.get('KUX_BINARY', str(ROOT / 'target/debug/kux'))).expanduser().resolve()

class Screen:
    def __init__(self, cols=140, rows=40):
        self.cols, self.rows, self.x, self.y = cols, rows, 0, 0
        self.cells = [[' '] * cols for _ in range(rows)]
        self.pending = ''
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
    def feed(self, data):
        s = self.pending + self.decoder.decode(data)
        self.pending = ''
        i = 0
        while i < len(s):
            ch = s[i]
            if ch == '\x1b':
                if i + 1 >= len(s): break
                if s[i+1] == '[':
                    match = re.match(r'\x1b\[([0-?]*)([ -/]*)([@-~])', s[i:])
                    if not match: break
                    raw, _, action = match.groups()
                    args = [int(v) if v.isdigit() else 0 for v in raw.lstrip('?=>').split(';')]
                    n = args[0] or 1
                    if action in 'Hf': self.y, self.x = (args[0] or 1)-1, (args[1] if len(args)>1 and args[1] else 1)-1
                    elif action == 'A': self.y -= n
                    elif action == 'B': self.y += n
                    elif action == 'C': self.x += n
                    elif action == 'D': self.x -= n
                    elif action == 'G': self.x = n-1
                    elif action == 'd': self.y = n-1
                    elif action == 'J':
                        mode = args[0]
                        if mode in (2,3): self.cells = [[' '] * self.cols for _ in range(self.rows)]
                        elif mode == 0:
                            self.cells[self.y][self.x:] = [' '] * (self.cols-self.x)
                            for y in range(self.y+1,self.rows): self.cells[y] = [' '] * self.cols
                        elif mode == 1:
                            self.cells[self.y][:self.x+1] = [' '] * (self.x+1)
                            for y in range(self.y): self.cells[y] = [' '] * self.cols
                    elif action == 'K':
                        mode = args[0]
                        if mode == 0: self.cells[self.y][self.x:] = [' '] * (self.cols-self.x)
                        elif mode == 1: self.cells[self.y][:self.x+1] = [' '] * (self.x+1)
                        elif mode == 2: self.cells[self.y] = [' '] * self.cols
                    elif action == 'X': self.cells[self.y][self.x:self.x+n] = [' '] * min(n,self.cols-self.x)
                    self.x, self.y = min(max(self.x,0),self.cols-1), min(max(self.y,0),self.rows-1)
                    i += len(match.group(0)); continue
                if s[i+1] == ']':
                    end = s.find('\x07',i+2)
                    st = s.find('\x1b\\',i+2)
                    if end < 0 and st < 0: break
                    i = end+1 if end>=0 and (st<0 or end<st) else st+2
                    continue
                if s[i+1] in '()':
                    if i+2>=len(s): break
                    i+=3;continue
                i+=2;continue
            if ch == '\r': self.x=0
            elif ch == '\n': self.y=min(self.y+1,self.rows-1)
            elif ch == '\b': self.x=max(self.x-1,0)
            elif ch == '\t': self.x=min((self.x//8+1)*8,self.cols-1)
            elif ord(ch) >= 32:
                width = 0 if unicodedata.combining(ch) else 2 if unicodedata.east_asian_width(ch) in ('W','F') else 1
                if width:
                    self.cells[self.y][self.x]=ch
                    if width==2 and self.x+1<self.cols: self.cells[self.y][self.x+1]=''
                    self.x=min(self.x+width,self.cols-1)
            i+=1
        self.pending=s[i:]
    def text(self): return '\n'.join(''.join(row).rstrip() for row in self.cells)

class Session:
    def __init__(self, name, extra=()):
        self.name=name; self.screen=Screen(); self.raw=bytearray(); self.step=0
        env=dict(os.environ)
        env.pop('NO_COLOR',None)
        env.update(TERM='xterm-256color',KUX_CONFIG_DIR=str(OUT/'config'),KUX_DATA_DIR=str(OUT/'data'),KUBECONFIG=str(OUT/'no-kubeconfig'),PATH=str(OUT/'bin')+os.pathsep+env.get('PATH',''),KUX_PTY_TRAP=str(OUT/'kubectl.log'))
        self.pid, self.fd=pty.fork()
        if self.pid == 0:
            os.chdir(ROOT)
            os.execve(BINARY,[str(BINARY),'--demo','--demo-items','300','--splashless','--refresh','30',*extra],env)
        fcntl.ioctl(self.fd,termios.TIOCSWINSZ,struct.pack('HHHH',40,140,0,0))
        self.read(0.8)
        self.wait(lambda text:'pods(all)[300]' in text and 'Context: demo' in text,'startup ready',timeout=10)
    def read(self,duration=0.15):
        end=time.monotonic()+duration
        while time.monotonic()<end:
            if not select.select([self.fd],[],[],max(0,end-time.monotonic()))[0]: break
            try: data=os.read(self.fd,65536)
            except OSError: break
            if not data: break
            self.raw.extend(data); self.screen.feed(data)
        return self.screen.text()
    def send(self,data,delay=0.18):
        os.write(self.fd,data.encode() if isinstance(data,str) else data)
        return self.read(delay)
    def wait(self,predicate,label,timeout=3):
        end=time.monotonic()+timeout
        while not predicate(self.screen.text()):
            if time.monotonic()>end:
                self.save('FAIL-'+label)
                raise AssertionError(label+'\n'+self.screen.text())
            self.read(0.05)
        return self.screen.text()
    def save(self,label):
        self.step+=1
        (OUT/f'{self.name}-{self.step:02d}-{label}.txt').write_text(self.screen.text())
    def close(self):
        self.send(b'\x03')
        try:
            end=time.monotonic()+2
            while time.monotonic()<end:
                pid,_=os.waitpid(self.pid,os.WNOHANG)
                if pid: break
                self.read(0.05)
            else: os.kill(self.pid,signal.SIGTERM);os.waitpid(self.pid,0)
        except ChildProcessError: pass
        os.close(self.fd)
        (OUT/f'{self.name}.ansi').write_bytes(self.raw)

def usedby(s,kind):
    s.send(':'+kind+'\r',0.25)
    s.send('u')
    return s.wait(lambda text:'UsedBy ' in text and '[5]' in text and 'GVR' in text,kind+' UsedBy')

def reference_rows(text):
    return [line for line in text.splitlines() if line.startswith('│') and re.search(r'(apps|batch)/v1/(deployments|daemonsets|statefulsets|jobs|cronjobs)',line)]

def check(condition,message):
    if not condition: raise AssertionError(message)

def flows(s):
    for kind in ['configmaps','secrets','persistentvolumeclaims','serviceaccounts','priorityclasses']:
        text=usedby(s,kind)
        header=next(line for line in text.splitlines() if 'NAMESPACE' in line and 'GVR' in line)
        check(header.index('NAMESPACE')<header.rindex('NAME')<header.index('GVR'),'exact column order')
        check(len(reference_rows(text))==5,'five workload rows '+kind)
        if kind=='priorityclasses':
            for index in range(5): check('team-%03d'%index in '\n'.join(reference_rows(text)),'actual PriorityClass namespaces')
        s.save(kind)
        s.send('\x1b')
        check('UsedBy ' not in s.screen.text(),'Escape source '+kind)
    usedby(s,'configmaps')
    rows=reference_rows(s.send('V'))
    ascending=[re.search(r'(?:apps|batch)/v1/\w+',line)[0] for line in rows]
    check(ascending==sorted(ascending),'ascending GVR sort')
    rows=reference_rows(s.send('V'))
    descending=[re.search(r'(?:apps|batch)/v1/\w+',line)[0] for line in rows]
    check(descending==sorted(descending,reverse=True),'descending GVR sort')
    text=s.send('/apps/v1/deployments\r')
    check(len(reference_rows(text))==1 and 'deploy-' in text,'filtered Deployment reference')
    s.save('filtered-deployment')
    text=s.send('\r',0.3)
    text=s.wait(lambda text:'deployments(team-000/deploy-0000000)[1]' in text,'Goto Deployment instance')
    check('team-000' in text,'Goto exact namespace')
    check('deployments(team-000/deploy-0000000)[1]' in text,'Goto exact instance view')
    check('deploy-0000250' not in text,'Goto excludes other namespace resources')
    s.save('goto-deployment')
    text=s.send('\x1b',0.3)
    check('UsedBy ' in text and len(reference_rows(text))==1,'Escape restores filtered References')
    check('/apps/v1/deployments' in text,'Escape restores filter')
    s.save('return-filtered')
    text=s.send('\x12',0.3)
    check('UsedBy ' in text and len(reference_rows(text))==1,'manual refresh preserves filter')
    s.send('h')
    check('UsedBy ' not in s.screen.text(),'h returns source')
    s.save('source-return')

def blocked_controls(s):
    text=usedby(s,'configmaps')
    keys=[b' ',b'\x04',b'\x0b',b'\x13',b'\x17',b'\x0c',b'\x1a',b'A',b'S',b'O',b'e',b'i',b'y',b'd',b'z',b'r',b't',b'0',b'1']
    for key in keys:
        text=s.send(key,0.05)
        check('UsedBy ' in text and len(reference_rows(text))==5,'suppressed key '+repr(key))
        check('●' not in '\n'.join(reference_rows(text)),'no marking '+repr(key))
        check('Confirm' not in text and 'Save' not in text and 'Screen Dumps' not in text,'no removed action '+repr(key))
    s.save('suppressed-controls')
    s.send('/apps/v1/deployments\r')
    s.send('\r',0.3)
    check('UsedBy ' not in s.screen.text() and 'deploy-0000000' in s.screen.text(),'read-only Goto')
    s.send('\x1b',0.3)
    check('UsedBy ' in s.screen.text(),'read-only return')
    s.save('readonly-return')

def faults(s):
    s.send(':persistentvolumeclaims\r',0.25)
    text=s.send('\x1a')
    check('persistentvolumeclaims-' in text,'fault source has pending claims')
    text=s.send('u',0.3)
    check('UsedBy ' in text and len(reference_rows(text))==5,'fault filter does not hide references')
    s.send('/apps/v1/deployments\r')
    text=s.send('\r',0.3)
    check('UsedBy ' not in text and 'deploy-' in text,'fault Goto target visible')
    check('deployments(team-000/deploy-0000000)[1]' in text,'Goto instance remains visible despite source fault filter')
    check('deploy-0000250' not in text,'Goto instance excludes unrelated resources')
    s.save('fault-goto')
    text=s.send('\x1b',0.3)
    check('UsedBy ' in text and len(reference_rows(text))==1,'fault return references')
    text=s.send('\x1b',0.3)
    check('UsedBy ' not in text and 'persistentvolumeclaims-' in text,'fault return source')
    s.save('fault-return-source')

def main():
    (OUT/'bin').mkdir()
    (OUT/'config').mkdir();(OUT/'data').mkdir()
    trap=OUT/'bin/kubectl'
    trap.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$KUX_PTY_TRAP"\nexit 91\n')
    trap.chmod(0o755)
    report={'created_at':datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),'binary':str(BINARY),'binary_sha256':hashlib.sha256(BINARY.read_bytes()).hexdigest(),'groups':[],'failures':[]}
    for name,extra,work in [('source-navigation',(),flows),('readonly-controls',('--read-only',),blocked_controls),('source-fault-filter',(),faults)]:
        s=Session(name,extra)
        try: work(s);report['groups'].append({'name':name,'passed':True})
        except Exception as exc:
            s.save('failure');report['failures'].append({'name':name,'error':str(exc)});report['groups'].append({'name':name,'passed':False})
        finally: s.close()
    report['kubectl_called']=(OUT/'kubectl.log').exists()
    if report['kubectl_called']:report['failures'].append({'name':'kubectl','error':'unexpected kubectl invocation'})
    (OUT/'report.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'report':str(OUT/'report.json'),'groups':report['groups'],'failures':report['failures'],'kubectl_called':report['kubectl_called']},indent=2))
    return bool(report['failures'])

if __name__ == '__main__': raise SystemExit(main())
