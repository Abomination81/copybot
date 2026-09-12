#!/usr/bin/env python3
"""Fail on common accidental inclusions. Reports locations, never matched values.

This is a release hygiene check, not a complete credential detector. Run an
independent scanner such as Gitleaks too. Never add private values to this script.
"""
from pathlib import Path
import ast
import json
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# Public protocol contracts and event signatures, not operator/leader wallets.
PUBLIC = {
    '4d97dcd97ec945f40cf65f87097ace5ea0476045',
    '2791bca1f2de4661ed88a30c99a7a9449aa84174',
    'e111180000d2663c0091e4f400237545b87b996b',
    'e2222d279d744050d28e00520010520000310f59',
    'ada100db00ca00073811820692005400218fce1f',
    'ada2005600dec949baf300f4c6120000bdb6eaab',
    'c011a7e12a19f7b1f670d46f03b03f3342e82dfb',
    'ada100874d00e3331d00f2007a9c336a65009718',
    'a238cbeb142c10ef7ad8442c6d1f9e89e07e7761',
    'c3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62',
    'd543adfd945773f1a62f74f0ee55a5e3b9b1a28262980ba90b1a8dcf9c1b0b0b',
    '4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb',
}
# Verified upstream CI action revisions and scanner release checksum.
PUBLIC_BUILD_DIGESTS = {
    '11d5960a326750d5838078e36cf38b85af677262',
    'a26af69be951a213d495a4c3e4e4022e16d87065',
    '551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb',
}
BUILD_DIRS = {'.git', 'target', '__pycache__', '.pytest_cache', '.venv', 'node_modules'}
FORBIDDEN_DIRS = {'data', 'run', 'control', 'archive', 'logs', 'ui-mockups'}
FORBIDDEN_EXT = {'.pem', '.key', '.p12', '.pfx', '.hex', '.jsonl', '.log', '.db', '.sqlite', '.sqlite3', '.csv', '.parquet', '.zip', '.gz', '.png', '.jpg', '.pdf', '.pyc'}
HEX = re.compile(r'(?<![0-9a-fA-F])(?:0x)?[0-9a-fA-F]{40,}(?![0-9a-fA-F])')
DECIMAL_ID = re.compile(r'\b\d{30,}\b')
KEY = re.compile(r'-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----|\b(?:gh[pousr]_|github_pat_|AKIA|ASIA|sk_live_|sk-proj-|re_)[A-Za-z0-9_-]{16,}')

def candidate_paths():
    if (ROOT / '.git').exists():
        raw = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=ROOT)
        return sorted({ROOT / p.decode() for p in raw.split(b'\0') if p})
    return sorted(p for p in ROOT.rglob('*') if p.is_file() and not BUILD_DIRS.intersection(p.relative_to(ROOT).parts))

def issues(path, text):
    rel = path.relative_to(ROOT)
    findings = []
    if FORBIDDEN_DIRS.intersection(rel.parts) or path.suffix.lower() in FORBIDDEN_EXT:
        findings.append(('excluded artifact type', 1))
    if path.name == 'copybot2.toml' or (path.name.endswith('.env') or path.name.startswith('.env')) and not path.name.endswith('.example'):
        findings.append(('populated configuration must not ship', 1))
    for label, rx in [('credential pattern', KEY), ('decimal market/token identifier', DECIMAL_ID)]:
        for m in rx.finditer(text):findings.append((label, text[:m.start()].count('\n') + 1))
    if path.name not in {'Cargo.lock'}:
        for m in HEX.finditer(text):
            bare = m.group().lower().removeprefix('0x')
            artificial = len(set(bare)) == 1 or (len(bare) == 40 and len(set(bare[:38])) == 1)
            if bare not in PUBLIC and bare not in PUBLIC_BUILD_DIGESTS and not artificial:
                findings.append(('unreviewed address/hash/payload', text[:m.start()].count('\n') + 1))
    if path.name.endswith('.env.example'):
        for number,line in enumerate(text.splitlines(), 1):
            if re.match(r'\s*(PRIVATE_KEY|.*API_KEY|.*SECRET|.*TOKEN|OUR_WALLET|WATCH_WALLET|FILLWATCH_FUNDER|ALERT_EMAIL_TO)\s*=', line):
                if line.split('=',1)[1].strip():findings.append(('credential/identity example must be empty', number))
    return findings

def main():
    bad=[];count=0
    for path in candidate_paths():
        rel=str(path.relative_to(ROOT))
        if path.is_symlink():bad.append((rel,1,'symlink not permitted'));continue
        try:text=path.read_text(encoding='utf-8')
        except (UnicodeError,OSError):bad.append((rel,1,'unreadable/non-text candidate'));continue
        count+=1
        bad.extend((rel,line,reason) for reason,line in issues(path,text))
        if path.suffix=='.py':
            try:ast.parse(text)
            except SyntaxError:bad.append((rel,1,'invalid Python syntax'))
    cfg=tomllib.loads((ROOT/'deploy/copybot2.example.toml').read_text())
    if cfg['bot']['mode']!='dry' or any(x['enabled'] for x in cfg['lane']):bad.append(('deploy/copybot2.example.toml',1,'example is not disabled/dry'))
    for file,line,reason in bad:print(f'{file}:{line}: {reason}')
    print(json.dumps({'candidate_files':count,'findings':len(bad)}))
    return bool(bad)

if __name__=='__main__':sys.exit(main())
