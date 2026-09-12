import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
try:
    import corroborate
    _CORROBORATE_REAL = True
except ImportError:

    class corroborate:
        CONFIRMED = 'CONFIRMED'
        REFUTED = 'REFUTED'
        UNVERIFIABLE = 'UNVERIFIABLE'
        STRANDED_MIN_USD = 25.0
        Ctx = staticmethod(lambda *a, **k: None)
        check = staticmethod(lambda *a, **k: ('CONFIRMED', 'corroboration unavailable'))
        halts = staticmethod(lambda v: True)
    _CORROBORATE_REAL = False
BOT_DIR = os.environ.get('BOT_DIR', '/opt/copybot')
BOT_PORT = int(os.environ.get('BOT_PORT', '8807'))
RUN_DIR = os.path.join(BOT_DIR, 'run')
STATE_PATH = os.path.join(RUN_DIR, 'fillwatch_state.json')
LOG_PATH = os.path.join(RUN_DIR, 'fillwatch.log')
ALERTS_PATH = os.environ.get('WATCHER_ALERTS', os.path.join(RUN_DIR, 'watcher_alerts.jsonl'))
RPC = os.environ.get('FILLWATCH_RPC', 'https://polygon.drpc.org')
CHUNK_BLOCKS = int(os.environ.get('FILLWATCH_CHUNK', '100'))
UA = 'Mozilla/5.0 (X11; Linux aarch64) fillwatch/1.0'
CTF = '0x4d97dcd97ec945f40cf65f87097ace5ea0476045'
TRANSFER_SINGLE = '0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62'
TRANSFER_BATCH = '0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb'
ZERO40 = '0' * 40
WINDOW_SECS = int(os.environ.get('FILLWATCH_WINDOW', '900'))
_BLOCK_SECS_ENV = os.environ.get('FILLWATCH_BLOCK_SECS')
BLOCK_SECS = float(_BLOCK_SECS_ENV) if _BLOCK_SECS_ENV else None

def measure_block_secs(sample=20000):
    global BLOCK_SECS
    if BLOCK_SECS:
        return BLOCK_SECS
    tip = int(rpc('eth_blockNumber', []), 16)
    lo = max(tip - sample, 1)
    try:
        t1 = int(rpc('eth_getBlockByNumber', [hex(tip), False])['timestamp'], 16)
        t0 = int(rpc('eth_getBlockByNumber', [hex(lo), False])['timestamp'], 16)
    except (TypeError, KeyError, ValueError) as e:
        raise Unverifiable('cannot read block headers to measure block time: %r' % (e,))
    span = tip - lo
    if span <= 0 or t1 <= t0:
        raise Unverifiable('cannot measure block time from the chain (tip %d)' % tip)
    BLOCK_SECS = (t1 - t0) / float(span)
    if not 0.2 <= BLOCK_SECS <= 10.0:
        raise Unverifiable('measured block time %.3fs is implausible' % BLOCK_SECS)
    return BLOCK_SECS
LAG_SECS = int(os.environ.get('FILLWATCH_LAG', '180'))
GRACE_SECS = int(os.environ.get('FILLWATCH_GRACE', '240'))
DUST_SHARES = float(os.environ.get('FILLWATCH_DUST', '1.0'))
DUST_USD = float(os.environ.get('FILLWATCH_DUST_USD', str(corroborate.STRANDED_MIN_USD)))

def log(msg):
    line = '%s %s' % (time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), msg)
    print(line, flush=True)
    try:
        os.makedirs(RUN_DIR, exist_ok=True)
        with open(LOG_PATH, 'a') as f:
            f.write(line + '\n')
    except OSError:
        pass

class Unverifiable(Exception):
    pass

def _post(url, payload, timeout=25):
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers={'content-type': 'application/json', 'User-Agent': UA})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())

def rpc(method, params):
    try:
        out = _post(RPC, {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params})
    except (urllib.error.URLError, OSError, ValueError) as e:
        raise Unverifiable('%s: %s' % (method, e))
    if 'error' in out:
        raise Unverifiable('%s: %s' % (method, out['error']))
    return out.get('result')

def pad_topic(addr):
    return '0x' + '0' * 24 + addr.lower().replace('0x', '')

def _chunked_logs(topics, lo, hi):
    out, start = ([], lo)
    while start <= hi:
        end = min(start + CHUNK_BLOCKS - 1, hi)
        res = rpc('eth_getLogs', [{'fromBlock': hex(start), 'toBlock': hex(end), 'address': [CTF], 'topics': topics}])
        if not isinstance(res, list):
            raise Unverifiable('eth_getLogs returned %r for %d-%d' % (type(res), start, end))
        out.extend(res)
        start = end + 1
    return out

def fills_in_range(wallets, lo, hi):
    pads = [pad_topic(w) for w in wallets]
    rows = []
    for sig in (TRANSFER_SINGLE, TRANSFER_BATCH):
        rows += _chunked_logs([sig, None, pads, None], lo, hi)
        rows += _chunked_logs([sig, None, None, pads], lo, hi)
    return rows

def decode_fill(ev, wallets):
    topics = ev.get('topics') or []
    if len(topics) < 4:
        return []
    frm, to = (topics[2][-40:].lower(), topics[3][-40:].lower())
    if frm == ZERO40 or to == ZERO40:
        return []
    if frm in wallets:
        owner, side = (frm, 1)
    elif to in wallets:
        owner, side = (to, 0)
    else:
        return []
    raw = (ev.get('data') or '0x')[2:]
    block = int(ev.get('blockNumber', '0x0'), 16)
    tx = ev.get('transactionHash', '')
    common = {'owner': '0x' + owner, 'side': side, 'block': block, 'tx': tx}
    if topics[0].lower() == TRANSFER_SINGLE:
        if len(raw) < 128:
            return []
        return [dict(common, token=str(int(raw[0:64], 16)), shares=int(raw[64:128], 16) / 1000000.0)]
    try:
        off_ids = int(raw[0:64], 16) * 2
        off_vals = int(raw[64:128], 16) * 2
        n_ids = int(raw[off_ids:off_ids + 64], 16)
        n_vals = int(raw[off_vals:off_vals + 64], 16)
        if n_ids != n_vals:
            return []
        out = []
        for i in range(n_ids):
            tid = int(raw[off_ids + 64 * (i + 1):off_ids + 64 * (i + 2)], 16)
            val = int(raw[off_vals + 64 * (i + 1):off_vals + 64 * (i + 2)], 16)
            out.append(dict(common, token=str(tid), shares=val / 1000000.0))
        return out
    except (ValueError, IndexError):
        raise Unverifiable('unparseable TransferBatch in tx %s' % tx)
BALANCE_OF = '0x00fdd58e'

def leader_balances(pairs):
    out = {}
    for owner, token in sorted(set(pairs)):
        data = BALANCE_OF + owner[2:].lower().rjust(64, '0') + format(int(token), '064x')
        raw = rpc('eth_call', [{'to': CTF, 'data': data}, 'latest'])
        if not isinstance(raw, str) or not raw.startswith('0x') or len(raw) < 3:
            raise Unverifiable('balanceOf(%s, …%s) unreadable' % (owner[:10], token[-8:]))
        out[owner.lower(), token] = int(raw, 16) / 1000000.0
    return out

def bot_state():
    try:
        req = urllib.request.Request('http://127.0.0.1:%d/api/pool' % BOT_PORT, headers={'User-Agent': UA})
        with urllib.request.urlopen(req, timeout=15) as r:
            pool = json.loads(r.read().decode())
        req = urllib.request.Request('http://127.0.0.1:%d/api/status' % BOT_PORT, headers={'User-Agent': UA})
        with urllib.request.urlopen(req, timeout=15) as r:
            status = json.loads(r.read().decode())
    except (urllib.error.URLError, OSError, ValueError) as e:
        raise Unverifiable('bot API: %s' % e)
    lanes = {}
    for l in pool.get('lanes') or []:
        name = l.get('name')
        if not name:
            continue
        st = (status.get('lanes') or {}).get(name) or {}
        lanes[name] = {'leader': str(l.get('leader') or '').lower(), 'armed': bool(l.get('armed')), 'ready': l.get('ready'), 'holdings': st.get('holdings') or {}}
    return lanes

def halt_buys(lane, reason, dry_run):
    if dry_run:
        log('  (dry-run — would halt BUYS on %s)' % lane)
        return True
    body = json.dumps({'lane': lane, 'state': 'halt_buys'}).encode()
    try:
        req = urllib.request.Request('http://127.0.0.1:%d/api/arm' % BOT_PORT, data=body, headers={'content-type': 'application/json', 'User-Agent': UA})
        with urllib.request.urlopen(req, timeout=15) as r:
            ok = json.loads(r.read().decode()).get('ok') is True
    except (urllib.error.URLError, OSError, ValueError) as e:
        log('  ⛔ FAILED to halt %s: %s' % (lane, e))
        return False
    log('  halted BUYS on %s (exits keep mirroring)' % lane)
    return ok

def position_marks():
    try:
        req = urllib.request.Request('http://127.0.0.1:%d/api/positions' % BOT_PORT, headers={'User-Agent': UA})
        with urllib.request.urlopen(req, timeout=15) as r:
            rows = json.loads(r.read().decode()).get('positions') or []
    except (urllib.error.URLError, OSError, ValueError) as e:
        log('  WARN: no position marks (%s) — every MISS will be judged as real' % e)
        return {}
    out = {}
    for p in rows:
        try:
            out[p.get('lane'), str(p.get('token'))] = float(p.get('mark'))
        except (TypeError, ValueError):
            continue
    return out

def is_unstrandable(lane, token, shares, marks):
    mark = marks.get((lane, str(token)))
    if mark is None or not mark > 0.0:
        return False
    return shares * mark < DUST_USD

def record_alert(kind, msg):
    seq = 0
    try:
        with open(ALERTS_PATH) as f:
            for line in f:
                try:
                    seq = max(seq, int(json.loads(line).get('seq') or 0))
                except ValueError:
                    continue
    except OSError:
        pass
    row = {'seq': seq + 1, 't': time.time(), 'level': 'CRITICAL', 'kind': kind, 'msg': msg, 'src': 'fillwatch'}
    try:
        os.makedirs(RUN_DIR, exist_ok=True)
        with open(ALERTS_PATH, 'a') as f:
            f.write(json.dumps(row) + '\n')
            f.flush()
            os.fsync(f.fileno())
    except OSError as e:
        log('  WARN: cannot persist alert: %s' % e)

def stranded_ctx(marks):
    funder = os.environ.get('FILLWATCH_FUNDER', '').lower() or None
    cache = {}

    def ours_on_chain(token):
        if not funder or not token:
            return None
        key = str(token)
        if key not in cache:
            try:
                bal = leader_balances([(funder, key)])
                cache[key] = bal.get((funder, key))
            except Exception:
                cache[key] = None
        return cache[key]

    def position_value(token):
        shares = ours_on_chain(token)
        if shares is None:
            return None
        for (_lane, tok), mark in (marks or {}).items():
            if str(tok) == str(token) and mark > 0.0:
                return shares * mark
        return None
    return corroborate.Ctx(our_positions=lambda: None, leader_positions=lambda _a: None, position_value=position_value, our_shares=ours_on_chain)

def find_faults(his, ours, lanes, now, balances, marks=None):
    marks = marks or {}
    _sctx = stranded_ctx(marks)
    faults = []
    for lane, info in lanes.items():
        leader = info['leader']
        held = {t: s for t, s in (info['holdings'] or {}).items() if s > DUST_SHARES}
        by_token = {}
        for f in his:
            if f['owner'] != leader or f['side'] != 1:
                continue
            if f['token'] not in held:
                continue
            agg = by_token.setdefault(f['token'], {'shares': 0.0, 'first_t': f['t'], 'last_t': f['t']})
            agg['shares'] += f['shares']
            agg['first_t'] = min(agg['first_t'], f['t'])
            agg['last_t'] = max(agg['last_t'], f['t'])
        for _tok, _agg in by_token.items():
            f = {'token': _tok, 'shares': _agg['shares'], 't': _agg['last_t'], 'first_t': _agg['first_t']}
            remaining = balances.get((leader, f['token']))
            if remaining is None:
                raise Unverifiable('no leader balance for …%s — cannot tell an exit from a trim' % f['token'][-8:])
            if remaining > DUST_SHARES:
                continue
            sold = any((o['side'] == 1 and o['token'] == f['token'] and (o['t'] >= f['first_t'] - 5) for o in ours))
            if not sold and now - f['t'] > GRACE_SECS:
                if is_unstrandable(lane, f['token'], held[f['token']], marks):
                    log('  dust MISS ignored: %s holds %.2f sh of …%s worth under $%.2f — nothing to be stranded in' % (lane, held[f['token']], f['token'][-8:], DUST_USD))
                    continue
                _v, _why = corroborate.check('stranded', {'token': f['token'], 'ours_shares': held[f['token']], 'lane': lane}, _sctx)
                if not corroborate.halts(_v):
                    log('  MISS REFUTED (not halting) %s …%s — %s' % (lane, f['token'][-8:], _why))
                    continue
                if _v != corroborate.CONFIRMED:
                    log('  (halting anyway — %s)' % _why)
                faults.append((lane, 'MISS', '%s EXITED …%s (sold %.2f sh, %.2f sh left) %.0fs ago and we did NOT — we still hold %.2f sh and the signal to leave has passed' % (lane, f['token'][-8:], f['shares'], remaining, now - f['t'], held[f['token']])))
        for o in ours:
            if o['side'] != 0:
                continue
            if o['token'] not in held:
                continue
            if any((f['token'] == o['token'] for f in his)):
                continue
            if (balances.get((leader, o['token'])) or 0.0) > DUST_SHARES:
                continue
            if now - o['t'] <= GRACE_SECS:
                continue
            faults.append((lane, 'HIT', 'we BOUGHT …%s (%.2f sh) with no leader fill in that market this window and %s holds none of it — a buy that traces to no leader is a decode defect or something else trading this wallet' % (o['token'][-8:], o['shares'], lane)))
            break
    return faults

def run(window_secs, dry_run):
    if not _CORROBORATE_REAL:
        log('WARN: corroborate.py is NOT importable here — MISS corroboration is INERT (it fails closed and halts exactly as before). Deploy it into this tree.')
    now = time.time()
    lanes = bot_state()
    if not lanes:
        log('PASS: no lanes configured — nothing to verify')
        return 0
    blk = measure_block_secs()
    tip = int(rpc('eth_blockNumber', []), 16)
    hi = tip - int(LAG_SECS / blk)
    lo = hi - int(window_secs / blk)
    log('block time measured at %.3fs — window is %d blocks' % (blk, hi - lo))
    if lo < 1 or hi <= lo:
        raise Unverifiable('nonsensical block range %d-%d (tip %d)' % (lo, hi, tip))
    leaders = sorted({i['leader'] for i in lanes.values() if i['leader']})
    funder = os.environ.get('FILLWATCH_FUNDER', '').lower() or None
    if not funder:
        raise Unverifiable('FILLWATCH_FUNDER is not set — cannot identify OUR fills')
    probe = rpc('eth_getLogs', [{'fromBlock': hex(max(lo, hi - CHUNK_BLOCKS + 1)), 'toBlock': hex(hi), 'address': [CTF], 'topics': [TRANSFER_SINGLE]}])
    if not isinstance(probe, list) or not probe:
        raise Unverifiable('the TransferSingle filter matched NOTHING on the CTF in %d blocks — the contract address or the event signature is wrong, or the RPC is not serving logs. Refusing to report a clean pass from a filter that finds nothing.' % CHUNK_BLOCKS)
    watched = {w.lower().replace('0x', '') for w in leaders + [funder]}
    raw = fills_in_range(leaders + [funder], lo, hi)

    def at(ev):
        return now - LAG_SECS - (hi - ev['block']) * blk
    decoded, seen = ([], set())
    for e in raw:
        for d in decode_fill(e, watched):
            key = (d['tx'], d['owner'], d['token'], d['side'], round(d['shares'], 6))
            if key in seen:
                continue
            seen.add(key)
            d['t'] = at(d)
            decoded.append(d)
    leader_set = {w.lower() for w in leaders}
    his = [d for d in decoded if d['owner'] in leader_set]
    ours = [d for d in decoded if d['owner'] == funder]
    log('window blocks %d-%d (%.0f min): his fills=%d ours=%d across %d lane(s)' % (lo, hi, window_secs / 60.0, len(his), len(ours), len(lanes)))
    wanted = set()
    for name, info in lanes.items():
        ldr = info['leader']
        held = {t for t, s in (info['holdings'] or {}).items() if s > DUST_SHARES}
        for f in his:
            if f['owner'] == ldr and f['side'] == 1 and (f['token'] in held):
                wanted.add((ldr, f['token']))
        for o in ours:
            if o['side'] == 0 and o['token'] in held:
                wanted.add((ldr, o['token']))
    balances = leader_balances(wanted)
    if wanted:
        log('leader balances checked: %d token(s) — %s' % (len(wanted), ', '.join(('…%s=%.2f' % (t[-8:], balances[w, t]) for w, t in sorted(wanted)))))
    faults = find_faults(his, ours, lanes, now, balances, position_marks())
    if not faults:
        log('PASS: every leader exit in the window is matched, and every buy of ours traces to a leader')
        return 0
    acted = True
    for lane, kind, msg in faults:
        log('⛔ %s (%s): %s' % (kind, lane, msg))
        record_alert('FILLWATCH_' + kind, msg)
        if not halt_buys(lane, msg, dry_run):
            acted = False
    if not acted:
        log('⛔ a halt could not be written — the fault stands and this pass did NOT consume it; the next run will retry')
        return 2
    return 1

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--once', action='store_true', help='ignored; always one pass')
    ap.add_argument('--dry-run', action='store_true')
    ap.add_argument('--window-secs', type=int, default=WINDOW_SECS)
    a = ap.parse_args()
    try:
        return run(a.window_secs, a.dry_run)
    except Unverifiable as e:
        log('UNVERIFIED (no conclusion drawn, nothing halted): %s' % e)
        return 2
    except Exception as e:
        log('FILLWATCH ITSELF FAILED: %r — nothing halted' % e)
        return 2
if __name__ == '__main__':
    sys.exit(main())
