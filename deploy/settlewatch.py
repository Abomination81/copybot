import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
import resolution as _resolution
BOT_DIR = os.environ.get('BOT_DIR', '/opt/copybot')
LEDGER = os.path.join(BOT_DIR, 'data', 'ledger.jsonl')
ERRORS = os.path.join(BOT_DIR, 'run', 'errors.jsonl')
EVENTS_GLOB = os.path.join(BOT_DIR, 'data', 'events-*.jsonl')
CLOB = 'https://clob.polymarket.com/markets/0x%s'
UA = {'User-Agent': 'copybot-settlewatch'}
RELEASE_RE = re.compile('released ([\\d.]+) sh of …(\\w+) at cost ([\\d.]+)')

def log(msg):
    print('%s %s' % (time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), msg), flush=True)

def _rows(path):
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    yield json.loads(line)
                except ValueError:
                    continue
    except OSError:
        return

def released_phantoms():
    agg = {}
    for e in _rows(LEDGER):
        if e.get('ev') != 'fill' or not e.get('recon'):
            continue
        try:
            if int(e.get('side') or 0) != 1:
                continue
            sh = float(e.get('shares') or 0.0)
            px = float(e.get('price') or 0.0)
        except (TypeError, ValueError):
            continue
        if sh <= 0:
            continue
        key = (e.get('lane'), str(e.get('token')))
        got = agg.get(key)
        if got:
            tot = got[0] + sh
            agg[key] = (tot, (got[0] * got[1] + sh * px) / tot if tot else px)
        else:
            agg[key] = (sh, px)
    return [(lane, sh, tok, px) for (lane, tok), (sh, px) in agg.items()]

def token_index():
    tails, conds = ({}, {})
    for e in _rows(LEDGER):
        t = str(e.get('token') or '')
        if t:
            tails[t[-10:]] = t
    import glob
    for path in sorted(glob.glob(EVENTS_GLOB)):
        for e in _rows(path):
            tok, cond = (e.get('tok'), e.get('condition'))
            if tok and cond:
                conds[str(tok)] = str(cond)
    return (tails, conds)

def already_booked(keys_wanted):
    seen = set()
    for e in _rows(LEDGER):
        if e.get('ev') != 'realised_adjust':
            continue
        for field in ('key', 'alt_key'):
            if e.get(field):
                seen.add(str(e[field]))
    return {k for k in keys_wanted if k in seen}

def resolution(condition):
    return (None, {})

def chain_payout(condition, token):
    return _resolution.settled_payout(condition, token)

def append_adjustment(lane, token, shares, payout, avg_cost, key, dry_run):
    proceeds = shares * payout
    row = {'ev': 'realised_adjust', 'lane': lane, 'token': token, 'shares': round(shares, 6), 'proceeds': round(proceeds, 6), 'avg_cost': avg_cost, 'pnl': round(proceeds - shares * avg_cost, 6), 'why': 'settlement missed by the redeemable-poll: the auto-redemption relayer swept this position before the booker saw it, and the reconciler then released it P&L-neutral', 'key': key, 't': int(time.time())}
    if dry_run:
        log('  WOULD BOOK %s' % json.dumps(row))
        return True
    try:
        with open(LEDGER, 'a') as f:
            f.write(json.dumps(row) + '\n')
            f.flush()
            os.fsync(f.fileno())
        return True
    except OSError as e:
        log('  could not append: %s' % e)
        return False

def run(dry_run=True, limit=None):
    rel = released_phantoms()
    if not rel:
        log('PASS: no released phantoms to check')
        return 0
    _tails, conds = token_index()
    keys = {}
    for lane, sh, tok, cost in rel:
        if tok:
            keys['settle:%s:%s' % (lane, tok)] = (lane, sh, tok, cost)
    done = already_booked(set(keys))
    todo = {k: v for k, v in keys.items() if k not in done}
    log('%d released phantom(s); %d already booked; %d to check' % (len(rel), len(done), len(todo)))
    booked = skipped = 0
    total = 0.0
    for key, (lane, sh, tok, cost) in sorted(todo.items()):
        cond = conds.get(tok)
        if not cond:
            log('  SKIP %s …%s — no conditionId in our own events' % (lane, tok[-8:]))
            skipped += 1
            continue
        pay = chain_payout(cond, tok)
        if pay is None:
            log('  SKIP %s …%s — the chain does not prove a settlement (unresolved or unreadable); will retry' % (lane, tok[-8:]))
            skipped += 1
            continue
        pnl = sh * pay - sh * cost
        if append_adjustment(lane, tok, sh, pay, cost, key, dry_run):
            booked += 1
            total += pnl
            log('  %s %-10s %8.1f sh @ payout %.4g (cost %.4f) -> realised %+.2f' % ('WOULD BOOK' if dry_run else 'BOOKED    ', lane, sh, pay, cost, pnl))
        if limit and booked >= limit:
            break
    log('%s %d settlement(s) worth %+.2f realised; %d skipped' % ('WOULD BOOK' if dry_run else 'BOOKED', booked, total, skipped))
    return 0

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--apply', action='store_true', help='actually write; default is a dry run')
    ap.add_argument('--limit', type=int, default=None)
    a = ap.parse_args()
    try:
        return run(dry_run=not a.apply, limit=a.limit)
    except Exception as e:
        log('SETTLEWATCH FAILED: %r — nothing written' % (e,))
        return 3
if __name__ == '__main__':
    sys.exit(main())
