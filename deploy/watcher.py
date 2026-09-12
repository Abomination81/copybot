import argparse
import base64
import collections
import json
import os
import socket
import ssl
import struct
import subprocess
import sys
import threading
import time
import urllib.request
try:
    import notify
    _NOTIFY_REAL = True
except ImportError:

    class notify:
        configured = staticmethod(lambda: False)
        send = staticmethod(lambda *a, **k: False)
        local_stamp = staticmethod(lambda t=None: '')
    _NOTIFY_REAL = False
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
HIM = os.environ.get('WATCH_WALLET', '').lower()
BOT_DIR = os.environ.get('BOT_DIR', '/opt/copybot')
BOT_PORT = int(os.environ.get('BOT_PORT', '8807'))
BOT_NAME = os.environ.get('BOT_NAME', 'copybot')
SERVICE = os.environ.get('BOT_SERVICE', 'copybot-hot.service')
PROCESS_NAME = os.environ.get('BOT_PROCESS', 'copybot-hot')
BOOT_GRACE_SECS = int(os.environ.get('BOOT_GRACE_SECS', '150'))
DEAD_CONFIRM = int(os.environ.get('DEAD_CONFIRM', '2'))
MARKET_COOLDOWN_SECS = 24 * 60 * 60
LIVE = 'ws-live-data.polymarket.com'
HEARTBEAT = os.environ.get('WATCHER_HEARTBEAT', os.path.join(BOT_DIR, 'run', 'watcher_heartbeat.json'))
FILLS_MAX = int(os.environ.get('WATCHER_FILLS_MAX', '5000'))
his_fills = collections.deque(maxlen=FILLS_MAX)
fills_dropped = 0
_dropped_alerted = 0
lock = threading.Lock()
ALERTS_PATH = os.environ.get('WATCHER_ALERTS', os.path.join(BOT_DIR, 'run', 'watcher_alerts.jsonl'))
_alert_seq = [0]

def append_alert(level, kind, msg, lanes=None):
    _alert_seq[0] += 1
    row = {'seq': _alert_seq[0], 't': time.time(), 'level': level, 'kind': kind, 'msg': msg}
    if lanes:
        row['lanes'] = sorted({str(x) for x in lanes if x})
    try:
        os.makedirs(os.path.dirname(ALERTS_PATH), exist_ok=True)
        with open(ALERTS_PATH, 'a') as f:
            f.write(json.dumps(row) + '\n')
            f.flush()
            os.fsync(f.fileno())
    except OSError as e:
        print('%s [WARN] cannot persist alert to %s: %s' % (time.strftime('%H:%M:%S'), ALERTS_PATH, e), flush=True)

def recv_exact(s, n):
    buf = b''
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise ConnectionError('peer closed mid-frame (%d/%d bytes)' % (len(buf), n))
        buf += chunk
    return buf

def _note_fill_eviction():
    global fills_dropped
    if len(his_fills) == his_fills.maxlen and his_fills:
        if not his_fills[0].get('seen'):
            fills_dropped += 1
FRAME_MAX_AGE_SECS = int(os.environ.get('FRAME_MAX_AGE_SECS', '180'))
DECODE_MAX_AGE_SECS = int(os.environ.get('DECODE_MAX_AGE_SECS', '300'))
DECODE_STALL_RESUBSCRIBE_SECS = int(os.environ.get('DECODE_STALL_RESUBSCRIBE_SECS', '20'))
RESUBSCRIBE_GRACE_SECS = int(os.environ.get('RESUBSCRIBE_GRACE_SECS', '45'))
_started_t = time.time()
_feed = {'connected': False, 'connected_since': None, 'resubscribing_since': None, 'last_frame_t': 0.0, 'last_decoded_t': 0.0, 'last_leader_fill_t': 0.0, 'reconnects': 0, 'last_error': None}
_feed_lock = threading.Lock()

def _feed_set(**kw):
    with _feed_lock:
        _feed.update(kw)

def _feed_bump(key):
    with _feed_lock:
        _feed[key] = _feed.get(key, 0) + 1

def feed_snapshot():
    now = time.time()
    with _feed_lock:
        f = dict(_feed)
    f['frame_age_secs'] = round(now - f['last_frame_t'], 1) if f['last_frame_t'] else None
    f['decoded_ever'] = bool(f['last_decoded_t'])
    f['decode_age_secs'] = round(now - (f['last_decoded_t'] or _started_t), 1)
    f['leader_fill_age_secs'] = round(now - f['last_leader_fill_t'], 1) if f['last_leader_fill_t'] else None
    frame_ref = f['last_frame_t'] or _started_t
    decode_ref = f['last_decoded_t'] or _started_t
    f['booting'] = not (f['last_frame_t'] and f['last_decoded_t'])
    healthy = bool(f['connected'] and now - frame_ref <= FRAME_MAX_AGE_SECS and (now - decode_ref <= DECODE_MAX_AGE_SECS))
    rs = f.get('resubscribing_since')
    f['resubscribing'] = bool(rs and now - rs <= RESUBSCRIBE_GRACE_SECS)
    f['useful'] = healthy or (f['resubscribing'] and now - decode_ref <= DECODE_MAX_AGE_SECS)
    return f

def watched_leaders():
    out = {}
    err = None
    path = os.path.join(BOT_DIR, 'run', 'control.json.wallets')
    try:
        with open(path) as f:
            doc = json.load(f)
        for w in (doc.get('wallets') if isinstance(doc, dict) else doc) or []:
            if w.get('enabled', True) and w.get('leader'):
                out[str(w['leader']).lower()] = str(w.get('name') or '?')
        if not out:
            err = 'wallets.json lists no enabled wallet'
    except FileNotFoundError:
        pass
    except (OSError, ValueError, TypeError, AttributeError) as e:
        err = 'cannot read %s: %s' % (os.path.basename(path), e)
    if not out and HIM:
        out[HIM] = 'default'
    return (out, err)

def write_heartbeat(who, problem):
    leaders, leaders_err = watched_leaders()
    os.makedirs(os.path.dirname(HEARTBEAT), exist_ok=True)
    tmp = HEARTBEAT + '.tmp'
    with open(tmp, 'w') as f:
        json.dump({'t': time.time(), 'armed': who, 'problem': problem, 'feed': feed_snapshot(), 'fills': {'depth': len(his_fills), 'cap': his_fills.maxlen, 'dropped_unjudged': fills_dropped}, 'leaders': sorted(leaders.values()), 'fill_lanes': sorted({str(f.get('leader_lane') or '') for f in list(his_fills) if f.get('leader_lane')}), 'leaders_error': leaders_err}, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, HEARTBEAT)
    directory = os.path.dirname(HEARTBEAT) or '.'
    fd = os.open(directory, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def _get(port, d, path='/api/status'):
    import urllib.request
    try:
        r = urllib.request.Request(f'http://127.0.0.1:{port}{path}')
        return json.load(urllib.request.urlopen(r, timeout=8))
    except Exception:
        return None

def operator_armed(bot_dir):
    try:
        with open(os.path.join(bot_dir, 'run', 'control.json.operator')) as f:
            lanes = json.load(f).get('lanes') or {}
        if not lanes:
            return None
        values = []
        for lane in lanes.values():
            if not isinstance(lane, dict) or not isinstance(lane.get('armed'), bool):
                return None
            values.append(lane['armed'])
        return any(values)
    except (OSError, ValueError, TypeError):
        return None

def operator_epoch(bot_dir):
    try:
        return os.stat(os.path.join(bot_dir, 'run', 'control.json.operator')).st_mtime
    except OSError:
        return None

def known_lanes(bot_dir):
    try:
        with open(os.path.join(bot_dir, 'run', 'control.json.operator')) as f:
            doc = json.load(f)
    except (OSError, ValueError):
        return set()
    return {str(n) for n in doc.get('lanes') or {}}

def halted_lanes(bot_dir):
    try:
        with open(os.path.join(bot_dir, 'run', 'control.json.operator')) as f:
            doc = json.load(f)
    except (OSError, ValueError):
        return set()
    out = set()
    for name, st in (doc.get('lanes') or {}).items():
        if isinstance(st, dict) and st.get('halt_buys'):
            out.add(str(name))
    return out

def runtime_liveness():
    alive = None
    try:
        state = subprocess.run(['systemctl', 'is-active', SERVICE], capture_output=True, text=True, timeout=6).stdout.strip()
        if state in ('active', 'activating', 'reloading'):
            alive = True
        elif state in ('inactive', 'failed', 'deactivating'):
            alive = False
    except Exception:
        alive = None
    uptime = None
    try:
        cols = subprocess.run(['ps', '-o', 'etimes=', '-C', PROCESS_NAME], capture_output=True, text=True, timeout=6).stdout.split()
        vals = [float(c) for c in cols if c.strip()]
        if vals:
            uptime = min(vals)
            if alive is None:
                alive = True
    except Exception:
        pass
    return (alive, uptime)
_dead_streak = 0

def armed_state():
    status = _get(BOT_PORT, BOT_DIR)
    intent = operator_armed(BOT_DIR)
    if status is None:
        return (None, None) if intent is False else (None, 'DEAD')
    if 'arm_requested' in status:
        armed = bool(status.get('arm_requested'))
    else:
        lanes = status.get('lanes') or {}
        armed = any((isinstance(l, dict) and l.get('armed') for l in lanes.values()))
    if armed:
        return (BOT_NAME, None)
    if intent is True:
        return (None, 'DEAD')
    return (None, None)

class EvidenceUnavailable(Exception):
    pass

def _reverse_lines(path, chunk_size=65536):
    with open(path, 'rb') as f:
        f.seek(0, os.SEEK_END)
        pos = f.tell()
        pending = b''
        while pos:
            take = min(chunk_size, pos)
            pos -= take
            f.seek(pos)
            pending = f.read(take) + pending
            parts = pending.split(b'\n')
            pending = parts[0]
            for line in reversed(parts[1:]):
                if line:
                    yield line.decode('utf-8', 'replace')
        if pending:
            yield pending.decode('utf-8', 'replace')
EVENT_ORDER_SLOP_SECS = 300
ARM_ACK_GRACE_SECS = 60

def _event_ts(event):
    try:
        raw = float(event.get('t', 0) or 0)
    except (TypeError, ValueError):
        return 0.0
    if raw <= 0:
        return 0.0
    return raw / 1000.0 if raw >= 100000000000 else raw

def _jsonl_events(directory, since_ts, kinds):
    out = []
    import glob
    try:
        paths = sorted(glob.glob(os.path.join(directory, 'data', 'events-*.jsonl')), reverse=True)
    except OSError as e:
        raise EvidenceUnavailable('cannot list %s/data: %s' % (directory, e))
    for path in paths:
        try:
            for line in _reverse_lines(path):
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                ts = _event_ts(e)
                if ts and ts < since_ts - EVENT_ORDER_SLOP_SECS:
                    return sorted(out, key=lambda row: row[0])
                if e.get('ev') in kinds and ts >= since_ts:
                    out.append((ts, e))
        except OSError as e:
            raise EvidenceUnavailable('cannot read %s: %s' % (os.path.basename(path), e))
    return sorted(out, key=lambda row: row[0])

def decisions_since(which, since_ts):
    out = []
    for ts, e in _jsonl_events(BOT_DIR, since_ts, {'fire', 'skip', 'signal_guard_skip'}):
        kind = str(e.get('ev', ''))
        out.append({'ts': ts, 'kind': kind, 'token': str(e.get('tok', '')), 'condition': str(e.get('condition', '')), 'side': 'BUY' if kind == 'signal_guard_skip' else str(e.get('side', '')).upper(), 'tx': str(e.get('tx', '')).lower(), 'his_order_id': str(e.get('his_order_id', '')), 'size': e.get('his_fill')})
    return out
fires_since = decisions_since
SIZE_MATCH_REL = 0.01
SIZE_MATCH_ABS = 0.5

def same_order_size(a, b):
    try:
        a, b = (float(a), float(b))
    except (TypeError, ValueError):
        return False
    return abs(a - b) <= max(SIZE_MATCH_ABS, SIZE_MATCH_REL * max(abs(a), abs(b)))

def token_matches(a, b):
    return bool(a and b and (a == b or a.startswith(b) or b.endswith(a[-12:])))
BUY_COVERAGE_TTL_SECS = int(os.environ.get('BUY_COVERAGE_TTL_SECS', str(24 * 60 * 60)))

def fill_is_covered(fill, decisions, grace):
    side = str(fill.get('side', '')).upper()
    token = str(fill.get('token', ''))
    condition = str(fill.get('condition', ''))
    tx = str(fill.get('tx', '')).lower()
    matching_side = [decision for decision in decisions if decision.get('side') == side]
    lo = float(fill.get('ts', 0)) - 30
    hi = float(fill.get('ts', 0)) + grace
    fill_size = float(fill.get('size', 0) or 0)
    for decision in matching_side:
        if decision.get('kind') != 'skip':
            continue
        if not lo <= float(decision.get('ts', 0)) <= hi:
            continue
        if not token_matches(token, decision.get('token', '')):
            continue
        decision_size = decision.get('size')
        if side != 'SELL' and decision_size is not None and (not same_order_size(float(decision_size), fill_size)):
            continue
        return True
    if tx and any((decision.get('kind') in (None, 'fire') and decision.get('tx') == tx and token_matches(token, decision.get('token', '')) for decision in matching_side)):
        return True
    if side == 'BUY':
        oldest = float(fill.get('ts', 0)) - BUY_COVERAGE_TTL_SECS
        durable = [decision for decision in matching_side if decision.get('kind') in (None, 'fire', 'signal_guard_skip') and float(decision.get('ts', 0) or 0) >= oldest]
        fill_order = str(fill.get('his_order_id', '') or '')
        if fill_order:
            same_order = [d for d in durable if d.get('his_order_id') == fill_order]
            if same_order:
                return True
            if any((d.get('his_order_id') for d in durable)):
                return False
        return any((condition and decision.get('condition') == condition or token_matches(token, decision.get('token', '')) for decision in durable))
    if side == 'SELL':
        return any((decision.get('kind') in (None, 'fire') and lo <= decision.get('ts', 0) <= hi and token_matches(token, decision.get('token', '')) for decision in matching_side))
    return False
GUARDIAN_STATE = os.path.join(BOT_DIR, 'run', 'guardian_state.json')
GUARDIAN_MAX_AGE_SECS = int(os.environ.get('GUARDIAN_MAX_AGE_SECS', '300'))
_guardian_reported = [False]

def guardian_liveness(path=None, now=None):
    path = path or GUARDIAN_STATE
    now = time.time() if now is None else now
    try:
        age = now - os.path.getmtime(path)
    except OSError:
        return ('UNKNOWN', 'no guardian state file at %s' % path, None)
    if age > GUARDIAN_MAX_AGE_SECS:
        return ('STALE', 'the guardian has not completed a run for %.0fs (it runs every ~11s) — the pool may be trading UNGUARDED' % age, age)
    return ('ALIVE', 'last run %.0fs ago' % age, age)

def check_guardian(alert):
    status, detail, _age = guardian_liveness()
    if status == 'STALE':
        if not _guardian_reported[0]:
            _guardian_reported[0] = True
            alert('WARN', 'GUARDIAN-DOWN', detail)
            if notify.configured():
                notify.send('Abomination81 Copybot: THE GUARDIAN IS NOT RUNNING', '%s\n\nThe bot is still trading and still mirroring exits — this is not a halt. What has stopped is the supervisor that reconciles the ledger against the chain and stops the pool when something is wrong.\n\n  systemctl status copybot-guardian.timer\n  systemctl start copybot-guardian.timer\n\n%s\n' % (detail, notify.local_stamp()), log=lambda m: alert('WARN', 'GUARDIAN-DOWN', m))
    elif status == 'ALIVE' and _guardian_reported[0]:
        _guardian_reported[0] = False
        alert('INFO', 'GUARDIAN-BACK', 'the guardian is completing runs again (%s)' % detail)
        if notify.configured():
            notify.send('Abomination81 Copybot: the guardian is running again', '%s\n%s\n' % (detail, notify.local_stamp()))

def _held_shares(token):
    try:
        req = urllib.request.Request('http://127.0.0.1:%d/api/positions' % BOT_PORT, headers={'User-Agent': 'copybot-watcher'})
        with urllib.request.urlopen(req, timeout=8) as r:
            doc = json.loads(r.read().decode())
        if doc.get('complete') is False or doc.get('status') == 'stale':
            return None
        rows = doc.get('positions') or []
    except Exception:
        return None
    total, seen = (0.0, False)
    for q in rows:
        if str(q.get('token')) == str(token):
            try:
                total += float(q.get('shares') or 0.0)
                seen = True
            except (TypeError, ValueError):
                return None
    return total if seen else 0.0

def held_tokens():
    status = _get(BOT_PORT, BOT_DIR)
    if not status:
        return None
    held = set()
    for lane in (status.get('lanes') or {}).values():
        if not isinstance(lane, dict) or not lane.get('armed'):
            continue
        for token, shares in (lane.get('holdings') or {}).items():
            try:
                if float(shares) > 0:
                    held.add(str(token))
            except (TypeError, ValueError):
                continue
    return held
_seen_rejects = collections.OrderedDict()
_POS_CACHE = {'t': 0.0, 'by': {}}
_POS_WARNED = False

def position_value(lane, token):
    if not lane or not token:
        return None
    now = time.time()
    if now - _POS_CACHE['t'] > 20:
        try:
            req = urllib.request.Request('http://127.0.0.1:%d/api/positions' % BOT_PORT, headers={'User-Agent': 'copybot-watcher'})
            with urllib.request.urlopen(req, timeout=8) as r:
                rows = json.loads(r.read().decode()).get('positions') or []
            by = {}
            for q in rows:
                try:
                    mark = float(q.get('mark'))
                    shares = float(q.get('shares') or 0.0)
                except (TypeError, ValueError):
                    continue
                if mark > 0.0:
                    by[q.get('lane'), str(q.get('token'))] = shares * mark
            _POS_CACHE.update({'t': now, 'by': by})
        except Exception as e:
            global _POS_WARNED
            if not _POS_WARNED:
                _POS_WARNED = True
                print('%s [WARN] cannot value positions (%r) — every reject will be judged as real until this clears' % (time.strftime('%H:%M:%S'), e), flush=True)
            _POS_CACHE.update({'t': now, 'by': {}})
            return None
    return _POS_CACHE['by'].get((lane, str(token)))

def rejects_since(which, since_ts):
    out = []
    for ts, e in _jsonl_events(BOT_DIR, since_ts, {'clob_resp'}):
        ok = e.get('ok')
        if ok is None:
            ok = (e.get('race') or {}).get('outcome') in ('matched', 'duplicate_only')
        if not ok:
            out.append((ts, e))
    out.sort(key=lambda x: x[0])
    return out
DUST_SELL_USD = float(os.environ.get('WATCHER_DUST_SELL_USD', '25.00'))

def check_rejects(which, alert, window=3600):
    for ts, e in rejects_since(which, time.time() - window):
        key = (int(ts * 1000), str(e.get('tok', ''))[-12:])
        if key in _seen_rejects:
            continue
        _seen_rejects[key] = True
        while len(_seen_rejects) > 400:
            _seen_rejects.popitem(last=False)
        why = ''
        for p in e.get('paths') or []:
            b = str(p.get('body', ''))
            if 'Duplicated' in b:
                continue
            why = b[:100]
            break
        if not why:
            why = 'all racing paths reported Duplicated — the order landed on another path'
        side = e.get('side', '?')
        outcome = (e.get('race') or {}).get('outcome', '?')
        no_liquidity = 'FAK' in why or 'no orders found' in why
        all_dupes = why.startswith('all racing paths')
        at_stake = position_value(e.get('lane'), e.get('tok'))
        dust_sell = side == 'SELL' and no_liquidity and (at_stake is not None) and (at_stake <= DUST_SELL_USD)
        preflight_refusal = any((m in (why or '').lower() for m in ('tick size', 'minimum tick', 'invalid price', 'minimum size', 'min size', 'size too small', 'breaks minimum')))
        level = 'WARN' if no_liquidity and side == 'BUY' or all_dupes or dust_sell or preflight_refusal else 'CRITICAL'
        if preflight_refusal:
            alert('WARN', 'BOT_DEFECT', f'we sent an order the venue could have refused on sight ({why}) — this is OURS to fix, not a reason to stop the pool')
        alert(level, 'REJECT', f"{side} …{str(e.get('tok', ''))[-10:]} limit={e.get('limit')} did not land ({outcome}): {why}" + ('  — WE MAY STILL BE HOLDING' if side == 'SELL' else ''))
RECONNECT_MIN_INTERVAL_SECS = 3.0
RECONNECT_MAX_INTERVAL_SECS = 60.0

def reconnect_delay(consecutive):
    steps = max(0, min(int(consecutive) - 1, 20))
    return min(RECONNECT_MIN_INTERVAL_SECS * 2 ** steps, RECONNECT_MAX_INTERVAL_SECS)

def ws_listen():
    consecutive = 0
    while True:
        try:
            ctx = ssl.create_default_context()
            s = ctx.wrap_socket(socket.create_connection((LIVE, 443), timeout=20), server_hostname=LIVE)
            k = base64.b64encode(os.urandom(16)).decode()
            s.send(f'GET / HTTP/1.1\r\nHost: {LIVE}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nOrigin: https://polymarket.com\r\nSec-WebSocket-Key: {k}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode())
            s.recv(4096)
            sub = json.dumps({'action': 'subscribe', 'subscriptions': [{'topic': 'activity', 'type': 'orders_matched'}]}).encode()
            m = os.urandom(4)
            h = bytearray([129])
            n = len(sub)
            if n < 126:
                h.append(128 | n)
            else:
                h.append(128 | 126)
                h += struct.pack('>H', n)
            s.send(bytes(h) + m + bytes((b ^ m[i % 4] for i, b in enumerate(sub))))

            def ctrl(opcode, payload=b''):
                mm = os.urandom(4)
                s.send(bytes([128 | opcode, 128 | len(payload)]) + mm + bytes((b ^ mm[i % 4] for i, b in enumerate(payload))))
            s.settimeout(25)
            _feed_set(connected=True, connected_since=time.time(), last_error=None, resubscribing_since=None)
            leaders, _ = watched_leaders()
            leaders_checked = time.time()
            while True:
                ref = max(float(_feed.get('last_decoded_t') or 0), float(_feed.get('connected_since') or 0))
                since_decode = time.time() - ref
                if ref and since_decode > DECODE_STALL_RESUBSCRIBE_SECS:
                    print('%s [WARN] no decodable fill for %.0fs on a live socket — resubscribing' % (time.strftime('%H:%M:%S'), since_decode), flush=True)
                    _feed_set(last_error='decode stall %.0fs — resubscribed' % since_decode, resubscribing_since=time.time())
                    break
                try:
                    hdr = s.recv(2)
                except socket.timeout:
                    ctrl(9, b'ka')
                    continue
                if len(hdr) < 2:
                    break
                op, ln = (hdr[0] & 15, hdr[1] & 127)
                if ln == 126:
                    ln = struct.unpack('>H', recv_exact(s, 2))[0]
                elif ln == 127:
                    ln = struct.unpack('>Q', recv_exact(s, 8))[0]
                buf = recv_exact(s, ln)
                _feed_set(last_frame_t=time.time())
                if op == 9:
                    ctrl(10, buf[:125])
                    continue
                if op not in (1, 2):
                    continue
                try:
                    v = json.loads(buf)
                except Exception:
                    continue
                p = v.get('payload') or {}
                if not isinstance(p, dict) or 'proxyWallet' not in p or 'asset' not in p:
                    continue
                _feed_set(last_decoded_t=time.time())
                consecutive = 0
                if time.time() - leaders_checked > 30:
                    leaders, _ = watched_leaders()
                    leaders_checked = time.time()
                who_lane = leaders.get(str(p.get('proxyWallet', '')).lower())
                if who_lane is None:
                    continue
                _feed_set(last_leader_fill_t=time.time())
                with lock:
                    _note_fill_eviction()
                    his_fills.append({'ts': time.time(), 'token': str(p.get('asset', '')), 'side': p.get('side'), 'size': float(p.get('size', 0)), 'condition': str(p.get('conditionId', '')), 'tx': str(p.get('transactionHash', '')), 'leader_lane': who_lane, 'title': (p.get('title') or '')[:44], 'seen': False})
        except Exception as e:
            _feed_set(connected=False, connected_since=None, last_error=repr(e)[:200])
            _feed_bump('reconnects')
            consecutive += 1
            time.sleep(reconnect_delay(consecutive))
        else:
            _feed_set(connected=False, connected_since=None)
            _feed_bump('reconnects')
            consecutive += 1
            time.sleep(reconnect_delay(consecutive))
FILLS_PATH = os.environ.get('WATCHER_FILLS', os.path.join(BOT_DIR, 'run', 'watcher_fills.json'))
FILLS_RETAIN_SECS = MARKET_COOLDOWN_SECS

def save_fills():
    cutoff = time.time() - FILLS_RETAIN_SECS
    with lock:
        rows = [f for f in his_fills if float(f.get('ts', 0)) >= cutoff]
    tmp = FILLS_PATH + '.%d.tmp' % os.getpid()
    try:
        os.makedirs(os.path.dirname(FILLS_PATH) or '.', exist_ok=True)
        with open(tmp, 'w') as f:
            json.dump({'fills': rows}, f)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, FILLS_PATH)
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise

def load_fills():
    try:
        with open(FILLS_PATH) as f:
            rows = (json.load(f) or {}).get('fills') or []
    except (OSError, ValueError, TypeError, AttributeError):
        return 0
    cutoff = time.time() - FILLS_RETAIN_SECS
    kept = 0
    with lock:
        for r in rows:
            if isinstance(r, dict) and float(r.get('ts', 0)) >= cutoff:
                _note_fill_eviction()
                his_fills.append(r)
                kept += 1
    return kept

def sweep(grace, alert):
    global _dead_streak
    who, problem = armed_state()
    if problem == 'DEAD':
        alive, uptime = runtime_liveness()
        booting = alive is True and (uptime is None or uptime < BOOT_GRACE_SECS)
        armed_since = operator_epoch(BOT_DIR)
        probe_unknown_but_fresh = alive is None and armed_since is not None and (time.time() - armed_since < ARM_ACK_GRACE_SECS)
        if booting or probe_unknown_but_fresh:
            _dead_streak = 0
            return
        _dead_streak += 1
        if _dead_streak < DEAD_CONFIRM:
            return
        alert('CRITICAL', 'DEAD', 'armed intent is not acknowledged by the runtime')
        return
    _dead_streak = 0
    if who is None:
        return
    now = time.time()
    armed_since = operator_epoch(BOT_DIR)
    if armed_since is None:
        alert('CRITICAL', 'STATE', 'cannot determine the current arm epoch')
        return
    try:
        check_rejects(who, alert)
    except EvidenceUnavailable as e:
        alert('CRITICAL', 'READ', "cannot read the bot's own event history: %s" % e)
        return
    with lock:
        for fill in his_fills:
            if not fill['seen'] and float(fill['ts']) < armed_since:
                fill['seen'] = True
        due = [f for f in his_fills if not f['seen'] and now - f['ts'] > grace]
    if not due:
        return
    try:
        fires = fires_since(who, now - MARKET_COOLDOWN_SECS)
    except EvidenceUnavailable as e:
        alert('CRITICAL', 'READ', "cannot read the bot's own event history: %s" % e)
        return
    holdings = held_tokens()
    halted = halted_lanes(BOT_DIR)
    for f in due:
        f['seen'] = True
        side = str(f.get('side', '')).upper()
        if side == 'SELL' and holdings is not None and (not any((token_matches(f['token'], token) for token in holdings))):
            continue
        if fill_is_covered(f, fires, grace):
            continue
        lane_of = str(f.get('leader_lane') or '')
        if lane_of:
            buying_was_off = lane_of in halted
        else:
            known = known_lanes(BOT_DIR)
            buying_was_off = bool(halted) and bool(known) and (known <= halted)
        if side == 'BUY' and buying_was_off:
            alert('INFO', 'MISS-SUPPRESSED', f"his BUY {f['size']:,.0f} in {f['title']} not copied — {lane_of or 'every lane'} is HALTED. Expected while stopped; clear the halt to resume buying.")
            continue
        if side == 'SELL':
            _tok = f['token']
            _v, _why = corroborate.check('stranded', {'token': _tok, 'lane': lane_of or who}, corroborate.Ctx(our_positions=lambda: None, leader_positions=lambda _a: None, position_value=lambda t: position_value(lane_of or who, t), our_shares=_held_shares))
            if not corroborate.halts(_v):
                alert('INFO', 'MISS-REFUTED', f"his SELL in {f['title']} not mirrored — {_why}")
                continue
            if _v != corroborate.CONFIRMED:
                print('%s [WARN] MISS halting without corroboration: %s' % (time.strftime('%H:%M:%S'), _why), flush=True)
        alert('CRITICAL', 'MISS', f"his {f['side']} {f['size']:,.0f} in {f['title']} — {who} did NOT fire within {grace}s", lanes=[f.get('leader_lane')] if f.get('leader_lane') else None)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--grace', type=int, default=45, help='seconds to allow between his fill and our fire')
    ap.add_argument('--once', action='store_true')
    a = ap.parse_args()
    worst = [0]

    def alert(level, kind, msg, lanes=None):
        line = f"{time.strftime('%H:%M:%S')} [{level}] {kind}: {msg}"
        print(line, flush=True)
        if level == 'CRITICAL':
            worst[0] = 2
            append_alert(level, kind, msg, lanes)
        elif worst[0] < 1:
            worst[0] = 1

    def guarded_sweep():
        try:
            sweep(a.grace, alert)
        except Exception as e:
            alert('CRITICAL', 'SWEEP', f'watcher sweep failed: {e!r}')
        try:
            check_guardian(alert)
        except Exception as e:
            alert('WARN', 'GUARDIAN-CHECK', f'could not check the guardian: {e!r}')
        try:
            save_fills()
        except OSError as e:
            alert('CRITICAL', 'FILLS', f'cannot persist the fill buffer: {e}')
        global _dropped_alerted
        if fills_dropped > _dropped_alerted:
            alert('CRITICAL', 'FILLS', f'{fills_dropped} unjudged leader fill(s) evicted by the {his_fills.maxlen}-row cap — MISS detection is INCOMPLETE; raise WATCHER_FILLS_MAX')
            _dropped_alerted = fills_dropped
    for _name, _real in (('corroborate', _CORROBORATE_REAL), ('notify', _NOTIFY_REAL)):
        if not _real:
            print('%s [WARN] %s.py is NOT importable here — the checks that depend on it are INERT (they fail closed, so nothing is less safe; they simply do nothing). Deploy it into this tree.' % (time.strftime('%H:%M:%S'), _name), flush=True)
    restored = load_fills()
    threading.Thread(target=ws_listen, daemon=True).start()
    who, problem = armed_state()
    lanes, _ = watched_leaders()
    print(f"watcher up — armed: {who or problem or 'neither'}  grace={a.grace}s  restored {restored} unjudged fill(s)  watching {len(lanes)} leader(s): {', '.join((f'{n}=…{w[-6:]}' for w, n in sorted(lanes.items(), key=lambda kv: kv[1]))) or 'NONE'}", flush=True)
    if a.once:
        time.sleep(a.grace + 15)
        guarded_sweep()
        who, problem = armed_state()
        write_heartbeat(who, problem)
        return worst[0]
    while True:
        time.sleep(15)
        guarded_sweep()
        try:
            who, problem = armed_state()
            write_heartbeat(who, problem)
        except OSError as e:
            alert('CRITICAL', 'HEARTBEAT', f'cannot persist watcher heartbeat: {e}')
if __name__ == '__main__':
    sys.exit(main() or 0)
