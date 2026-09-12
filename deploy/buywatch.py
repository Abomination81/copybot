import json
import os
import sys
import time
import glob
import collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fillwatch as fw
BOT_DIR = os.environ.get('BOT_DIR', '/opt/copybot')
WINDOW_SECS = int(os.environ.get('BUYWATCH_WINDOW', '1260'))
JOIN_SECS = int(os.environ.get('BUYWATCH_JOIN', '900'))
ALERT_PATH = os.path.join(BOT_DIR, 'run', 'buywatch_alerts.jsonl')

def log(msg):
    print('%s %s' % (time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), msg), flush=True)

def lane_armed_since():
    path = os.path.join(BOT_DIR, 'run', 'control.json.operator')
    out = {}
    try:
        with open(path) as f:
            doc = json.load(f)
    except (OSError, ValueError):
        return out
    for name, v in (doc.get('lanes') or {}).items():
        if isinstance(v, dict) and isinstance(v.get('at'), (int, float)):
            out[str(name)] = float(v['at'])
    return out

def our_decisions(window_secs, now):
    out = collections.defaultdict(list)
    for path in sorted(glob.glob(os.path.join(BOT_DIR, 'data', 'events-*.jsonl'))):
        try:
            fh = open(path)
        except OSError:
            continue
        with fh:
            for line in fh:
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                if e.get('ev') not in ('fire', 'skip', 'signal_guard_skip'):
                    continue
                t = (e.get('t') or 0) / 1000.0
                if now - t > window_secs + JOIN_SECS + 600:
                    continue
                out[e.get('lane'), str(e.get('tok') or '')[:14]].append((t, e))
    return out

def classify(buy, lane, decisions):
    near = [(t, e) for t, e in decisions.get((lane, str(buy['token'])[:14]), []) if abs(t - buy['t']) <= JOIN_SECS and str(e.get('side') or '').upper() != 'SELL']
    if any((e.get('ev') == 'fire' and str(e.get('side')).upper() == 'BUY' for _, e in near)):
        return 'FIRED'
    for _, e in near:
        if e.get('ev') in ('skip', 'signal_guard_skip'):
            why = str(e.get('why') or 'skip')
            return 'DECLINED:' + why.split('{')[0].split('(')[0].strip()
    if near:
        return 'DECLINED:unlabelled'
    return 'UNSEEN'

def run(window_secs, dry_run=False):
    now = time.time()
    lanes = fw.bot_state()
    if not lanes:
        log('PASS: no lanes configured — nothing to verify')
        return 0
    by_leader = {v['leader'].lower(): k for k, v in lanes.items() if v.get('leader')}
    if not by_leader:
        log('UNVERIFIED: no lane has a leader address')
        return 3
    blk = fw.measure_block_secs()
    tip = int(fw.rpc('eth_blockNumber', []), 16)
    hi = tip - int(fw.LAG_SECS / blk)
    lo = hi - int(window_secs / blk)
    probe = fw.rpc('eth_getLogs', [{'fromBlock': hex(max(lo, hi - fw.CHUNK_BLOCKS + 1)), 'toBlock': hex(hi), 'address': [fw.CTF], 'topics': [fw.TRANSFER_SINGLE]}])
    if not isinstance(probe, list) or not probe:
        log('UNVERIFIED: the CTF transfer filter matched NOTHING in %d blocks — refusing to report a clean pass from a filter that finds nothing' % fw.CHUNK_BLOCKS)
        return 3
    watched = {w.lower().replace('0x', '') for w in by_leader}
    raw = fw.fills_in_range(list(by_leader), lo, hi)
    seen, buys = (set(), [])
    for ev in raw:
        for d in fw.decode_fill(ev, watched):
            key = (d['tx'], d['owner'], d['token'], d['side'], round(d['shares'], 6))
            if key in seen:
                continue
            seen.add(key)
            if d['side'] != 0:
                continue
            d['t'] = now - fw.LAG_SECS - (hi - d['block']) * blk
            buys.append(d)
    log('block time %.3fs · window %d blocks (%.0f min) · %d leader BUYS' % (blk, hi - lo, window_secs / 60.0, len(buys)))
    if not buys:
        log('PASS: no leader bought anything in the window — nothing to copy')
        return 0
    decisions = our_decisions(window_secs, now)
    armed_since = lane_armed_since()
    tally = collections.Counter()
    unseen = []
    prenatal = collections.Counter()
    for b in buys:
        lane = by_leader.get(str(b['owner']).lower(), '?')
        since = armed_since.get(lane)
        if since is not None and b['t'] < since:
            prenatal[lane] += 1
            continue
        v = classify(b, lane, decisions)
        tally[lane, v] += 1
        if v == 'UNSEEN':
            unseen.append((lane, b))
    for lane, n in sorted(prenatal.items()):
        log('  %-10s %d buy(s) predate this lane being armed — not a miss, skipped' % (lane, n))
    for (lane, v), n in sorted(tally.items()):
        log('  %-10s %-34s %d' % (lane, v, n))
    fired = sum((n for (_, v), n in tally.items() if v == 'FIRED'))
    judged = len(buys) - sum(prenatal.values())
    if judged <= 0:
        log('PASS: every leader buy in the window predates its lane — nothing to copy yet')
        return 0
    log('copied %d of %d leader buys (%.0f%%)' % (fired, judged, 100.0 * fired / judged))
    if unseen:
        for lane, b in unseen[:10]:
            log("!!! UNSEEN: %s — leader bought %.2f sh of …%s and the bot recorded NO decision. The decode did not surface his fill; check the feed and that this lane's leader address is right. tx %s" % (lane, b['shares'], str(b['token'])[-8:], b.get('tx')))
        if not dry_run:
            try:
                os.makedirs(os.path.dirname(ALERT_PATH), exist_ok=True)
                with open(ALERT_PATH, 'a') as f:
                    f.write(json.dumps({'t': int(now), 'kind': 'unseen_leader_buys', 'count': len(unseen), 'lanes': sorted({l for l, _ in unseen}), 'sample': [{'lane': l, 'token': str(b['token']), 'shares': b['shares'], 'tx': b.get('tx')} for l, b in unseen[:10]]}) + '\n')
            except OSError as e:
                log('could not write the alert file: %s' % e)
        log('NOT-PASS: %d leader buy(s) never reached a decision' % len(unseen))
        return 1
    log('PASS: every leader buy in the window reached a decision')
    return 0

def main():
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument('--once', action='store_true', help='ignored; always one pass')
    ap.add_argument('--dry-run', action='store_true')
    ap.add_argument('--window-secs', type=int, default=WINDOW_SECS)
    a = ap.parse_args()
    try:
        return run(a.window_secs, a.dry_run)
    except fw.Unverifiable as e:
        log('UNVERIFIED (nothing halted, nothing concluded): %s' % e)
        return 3
    except Exception as e:
        log('BUYWATCH ITSELF FAILED: %r — nothing halted' % (e,))
        return 3
if __name__ == '__main__':
    sys.exit(main())
