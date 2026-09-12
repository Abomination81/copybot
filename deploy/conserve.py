import argparse
import json
import os
import sys
import time
import urllib.request
BOT_DIR = os.environ.get('BOT_DIR', '/opt/copybot')
LEDGER = os.path.join(BOT_DIR, 'data', 'ledger.jsonl')
FUNDING = os.path.join(BOT_DIR, 'run', 'control.json.funding')
STATUS = os.environ.get('COPYBOT_API', 'http://127.0.0.1:8807')
EXIT_OK = 0
EXIT_DIVERGED = 1
EXIT_UNKNOWN = 2
TOLERANCE_USD = 25.0
TOLERANCE_FRAC = 0.002

def log(msg):
    print('%s %s' % (time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), msg), flush=True)

def rows(path):
    try:
        with open(path, errors='replace') as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    yield json.loads(line)
                except ValueError:
                    continue
    except OSError:
        return

def realised_in_window(since, until):
    total = 0.0
    fills = 0
    adjust = 0.0
    corrections = 0.0
    fees = 0.0
    spent = 0.0
    cost_of_sold = 0.0
    pos = {}
    for e in rows(LEDGER):
        t = e.get('t') or 0
        ev = e.get('ev')
        if ev == 'realised_adjust':
            if since <= t < until:
                try:
                    r_sh = float(e.get('shares') or 0.0)
                    r_pr = float(e.get('proceeds') or 0.0)
                    r_av = float(e.get('avg_cost') or 0.0)
                except (TypeError, ValueError):
                    r_sh = r_pr = r_av = 0.0
                if r_sh == 0.0 and r_pr == 0.0:
                    corrections += float(e.get('pnl') or 0.0)
                    continue
                adjust += float(e.get('pnl') or 0.0)
                cost_of_sold += r_sh * r_av
            continue
        if ev != 'fill':
            continue
        key = (e.get('lane'), str(e.get('token')))
        try:
            sh = float(e.get('shares') or 0.0)
            px = float(e.get('price') or 0.0)
            side = int(e.get('side') or 0)
        except (TypeError, ValueError):
            continue
        if e.get('recon'):
            if side == 1:
                held, cost = pos.get(key, (0.0, 0.0))
                take = min(sh, held)
                avg = cost / held if held > 1e-09 else 0.0
                pos[key] = (held - take, cost - take * avg)
            continue
        held, cost = pos.get(key, (0.0, 0.0))
        if side == 0:
            if since <= t < until:
                spent += sh * px
                fees += float(e.get('fee') or 0.0)
            pos[key] = (held + sh, cost + sh * px)
        else:
            avg = cost / held if held > 1e-09 else 0.0
            take = min(sh, held)
            if since <= t < until:
                total += take * (px - avg)
                cost_of_sold += take * avg
                fills += 1
                fees += float(e.get('fee') or 0.0)
            pos[key] = (held - take, cost - take * avg)
    return (total, adjust, fees, fills, spent, cost_of_sold, corrections)

def funding_in_window(since, until):
    total = 0.0
    n = 0
    for e in rows(FUNDING):
        t = e.get('t') or 0
        if since <= t < until:
            total += float(e.get('usd') or 0.0)
            n += 1
    return (total, n)

def equity_now():
    try:
        req = urllib.request.Request(STATUS + '/api/pool', headers={'User-Agent': 'copybot-conserve'})
        with urllib.request.urlopen(req, timeout=15) as r:
            d = json.load(r)
    except Exception:
        return (None, None)
    w = d.get('wallet') or {}
    return (w.get('portfolio'), d.get('physical_cash'))

def run(days):
    until = int(time.time())
    equity, cash = equity_now()
    if equity is None:
        log('── conservation check ──')
        log('  venue equity                   :          ? (api unreachable)')
        log("VERDICT: UNKNOWN — cannot check the identity without the venue's equity")
        return EXIT_UNKNOWN
    base = read_baselines()
    prior = pick_baseline(base, until - days * 86400)
    record_baseline(until, equity, cash)
    if prior is None:
        log('── conservation check ──')
        log('VERDICT: BASELINE RECORDED — no earlier equity reading covers this window.')
        log('         Nothing to check against yet; the next run has a verdict.')
        return EXIT_OK
    since = int(prior['t'])
    realised, adjust, fees, fills, spent, cost_of_sold, corrections = realised_in_window(since, until)
    funded, nfund = funding_in_window(since, until)
    age_h = (until - since) / 3600.0
    prior_cash = prior.get('cash')
    if not isinstance(prior_cash, (int, float)) or not isinstance(cash, (int, float)):
        log('── conservation check over the last %.1fh ──' % age_h)
        log('VERDICT: UNKNOWN — the baseline predates cash journalling, so the mark')
        log('         movement cannot be reconstructed. The next run has a verdict.')
        return EXIT_UNKNOWN
    posval_now = equity - cash
    posval_base = prior['equity'] - prior_cash
    mark_move = posval_now - posval_base - spent + cost_of_sold
    log('── conservation check over the last %.1fh ──' % age_h)
    log('  realised P&L from fills        : %+10.2f  (%d closing fills)' % (realised, fills))
    log('  settlement adjustments booked  : %+10.2f' % adjust)
    log('  fees booked (both sides)       : %+10.2f' % -fees)
    log('  mark movement on open positions: %+10.2f' % mark_move)
    log('  external funding declared      : %+10.2f  (%d entr%s)' % (funded, nfund, 'y' if nfund == 1 else 'ies'))
    accounted = realised + adjust - fees + mark_move
    log('  ---------------------------------------------')
    log('  our books say the account moved: %+10.2f' % (accounted + funded))
    if corrections:
        log('  (excluded: %+.2f of P&L-only correction rows — zero cash, zero shares;' % corrections)
        log("   they fix an EARLIER window's books and are not this window's divergence)")
    expected = prior['equity'] + funded + accounted
    gap = equity - expected
    tol = max(TOLERANCE_USD, abs(prior['equity']) * TOLERANCE_FRAC)
    log('')
    log('  equity %.1fh ago                : %10.2f' % (age_h, prior['equity']))
    log('  = expected equity now          : %10.2f' % expected)
    log('  actual equity now              : %10.2f' % equity)
    log('  ---------------------------------------------')
    log('  UNEXPLAINED                    : %+10.2f  (tolerance %.2f)' % (gap, tol))
    log('')
    if abs(gap) <= tol:
        log('VERDICT: BALANCED — the books describe the account over this window.')
        return EXIT_OK
    log('VERDICT: DIVERGED by %+.2f — something moved money our fills do not explain.' % gap)
    log('         Usual suspects, in order: positions the venue could not mark, a')
    log('         redemption booked at cost awaiting chain proof (settlewatch retries')
    log('         these every 30 min), an undeclared deposit or withdrawal, or a fee')
    log('         the venue charged and we did not record.')
    return EXIT_DIVERGED
BASELINES = os.path.join(BOT_DIR, 'run', 'equity_baseline.jsonl')

def read_baselines():
    out = []
    for e in rows(BASELINES):
        if isinstance(e.get('t'), (int, float)) and isinstance(e.get('equity'), (int, float)):
            out.append(e)
    out.sort(key=lambda x: x['t'])
    return out

def pick_baseline(base, since):
    older = [b for b in base if b['t'] <= since]
    return older[-1] if older else None
NO_RECORD = False

def record_baseline(t, equity, cash):
    if NO_RECORD:
        log('  (--no-record: baseline NOT written)')
        return
    try:
        os.makedirs(os.path.dirname(BASELINES), exist_ok=True)
        with open(BASELINES, 'a') as fh:
            fh.write(json.dumps({'t': int(t), 'equity': equity, 'cash': cash}) + '\n')
            fh.flush()
            os.fsync(fh.fileno())
    except OSError as e:
        log('  (could not record baseline: %s)' % e)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--days', type=int, default=1)
    ap.add_argument('--notify', action='store_true', help='on DIVERGED, send one email via notify.py (timer mode)')
    ap.add_argument('--no-record', action='store_true', help='do not append a baseline reading — for inspecting the books without changing them')
    a = ap.parse_args()
    global NO_RECORD
    NO_RECORD = a.no_record
    try:
        rc = run(max(1, a.days))
    except Exception as e:
        log('CONSERVE FAILED: %r — nothing written (this tool never writes)' % (e,))
        return 3
    if rc == EXIT_DIVERGED and a.notify:
        try:
            import notify
            if notify.configured():
                notify.send('copybot: money-conservation check DIVERGED', 'conserve.py found equity movement the fills do not explain over the last %dd window. Run `python3 deploy/conserve.py --days %d` on the box for the full breakdown. Nothing was halted; this is the check that turns week-three archaeology into day-one questions.\n%s\n' % (a.days, a.days, notify.local_stamp()), log=log)
            else:
                log('(--notify set but notify.py is not configured: %s)' % notify.why_not_configured())
        except Exception as e:
            log('(divergence email failed, verdict unchanged: %r)' % (e,))
    return rc
if __name__ == '__main__':
    sys.exit(main())
