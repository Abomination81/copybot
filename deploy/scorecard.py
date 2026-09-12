import json
import os
import time
TRUE_POSITIVE = 'TRUE_POSITIVE'
FALSE_POSITIVE = 'FALSE_POSITIVE'
UNKNOWN = 'UNKNOWN'
ADJUDICATE_AFTER_SECS = 1200
STILL_OPEN_SECS = 24 * 3600
MAX_CASES = 500

def _read(path):
    try:
        with open(path) as f:
            rows = [json.loads(l) for l in f if l.strip()]
        return [r for r in rows if isinstance(r, dict)]
    except (OSError, ValueError):
        return []

def _write(path, rows):
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        tmp = path + '.tmp'
        with open(tmp, 'w') as f:
            for r in rows[-MAX_CASES:]:
                f.write(json.dumps(r) + '\n')
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
        return True
    except OSError:
        return False

def open_case(path, lanes, meta, corroboration, now):
    rows = _read(path)
    if rows and rows[-1].get('verdict') is None and (rows[-1].get('cleared_at') is None):
        return None
    case = {'at': now, 'lanes': sorted(lanes or []), 'codes': sorted((meta or {}).get('codes') or []), 'why': str((meta or {}).get('why') or '')[:300], 'by': str((meta or {}).get('by') or ''), 'corroboration': corroboration, 'cleared_at': None, 'buys_declined': None, 'verdict': None}
    rows.append(case)
    _write(path, rows)
    return case

def close_case(path, now, buys_declined=None):
    rows = _read(path)
    for r in reversed(rows):
        if r.get('cleared_at') is None:
            r['cleared_at'] = now
            if buys_declined is not None:
                r['buys_declined'] = int(buys_declined)
            _write(path, rows)
            return r
    return None

def adjudicate(path, now, still_faulting):
    rows = _read(path)
    scored = []
    for r in rows:
        if r.get('verdict') is not None:
            continue
        if now - float(r.get('at') or 0) < ADJUDICATE_AFTER_SECS:
            continue
        if not r.get('cleared_at') and now - float(r.get('at') or 0) < STILL_OPEN_SECS:
            continue
        r['verdict'], r['verdict_why'] = _judge(r, still_faulting)
        r['judged_at'] = now
        scored.append(r)
    if scored:
        _write(path, rows)
    return scored

def _judge(case, still_faulting):
    corr = case.get('corroboration')
    if corr == 'CONFIRMED':
        return (TRUE_POSITIVE, 'an independent source confirmed the breach at halt time')
    try:
        persists = still_faulting(case)
    except Exception as e:
        return (UNKNOWN, 'could not re-check the fault: %r' % (e,))
    if persists is None:
        return (UNKNOWN, 'the fault could not be re-checked')
    if persists:
        return (TRUE_POSITIVE, 'the fault was still present when re-checked')
    cost = case.get('buys_declined')
    if not case.get('cleared_at'):
        return (UNKNOWN, 'the fault has gone but the halt has not — outlived its cause, still open')
    if isinstance(cost, int) and cost > 0:
        return (FALSE_POSITIVE, 'the fault was gone on re-check, nothing independent ever confirmed it, and it declined %d leader buy(s)' % cost)
    return (UNKNOWN, 'the fault was gone on re-check but it cost nothing measurable — not worth calling wrong')

def summarise(path, now, window_secs=7 * 86400):
    rows = [r for r in _read(path) if now - float(r.get('at') or 0) <= window_secs and r.get('verdict')]
    n = {TRUE_POSITIVE: 0, FALSE_POSITIVE: 0, UNKNOWN: 0}
    cost = 0
    for r in rows:
        n[r['verdict']] = n.get(r['verdict'], 0) + 1
        if r['verdict'] == FALSE_POSITIVE and isinstance(r.get('buys_declined'), int):
            cost += r['buys_declined']
    judged = n[TRUE_POSITIVE] + n[FALSE_POSITIVE]
    precision = 100.0 * n[TRUE_POSITIVE] / judged if judged else None
    return {'cases': len(rows), 'true': n[TRUE_POSITIVE], 'false': n[FALSE_POSITIVE], 'unknown': n[UNKNOWN], 'buys_lost_to_false_halts': cost, 'precision_pct': precision}

def summary_line(s):
    p = '%.0f%%' % s['precision_pct'] if s['precision_pct'] is not None else 'n/a'
    return 'halt precision %s over %d judged case(s): %d true, %d false, %d unknown; %d leader buy(s) lost to false halts' % (p, s['true'] + s['false'], s['true'], s['false'], s['unknown'], s['buys_lost_to_false_halts'])
