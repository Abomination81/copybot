import json
import os
import urllib.error
import urllib.request
CTF = os.environ.get('CTF_ADDRESS', '0x4d97dcd97ec945f40cf65f87097ace5ea0476045')
RPC_URL = os.environ.get('FILLWATCH_RPC', 'https://polygon.drpc.org')
SEL_DENOMINATOR = '0xdd34de67'
SEL_NUMERATOR = '0x0504c814'
MIN_PAYOUT, MAX_PAYOUT = (0.0, 1.0)

def _rpc(method, params, timeout=12):
    body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}).encode()
    req = urllib.request.Request(RPC_URL, data=body, method='POST', headers={'Content-Type': 'application/json', 'User-Agent': 'copybot-resolution'})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        out = json.loads(r.read().decode())
    if 'error' in out:
        raise ValueError(str(out['error'])[:120])
    return out.get('result')

def _hex32(condition):
    c = str(condition or '')
    if c.startswith('0x'):
        c = c[2:]
    if len(c) != 64 or any((ch not in '0123456789abcdefABCDEF' for ch in c)):
        return None
    return c.lower()

def payout_denominator(condition):
    c = _hex32(condition)
    if not c:
        return None
    try:
        raw = _rpc('eth_call', [{'to': CTF, 'data': SEL_DENOMINATOR + c}, 'latest'])
        return int(raw, 16) if isinstance(raw, str) and raw.startswith('0x') else None
    except (urllib.error.URLError, OSError, ValueError, TypeError):
        return None

def is_resolved(condition):
    den = payout_denominator(condition)
    if den is None:
        return None
    return den > 0

def payout_for_index(condition, index):
    c = _hex32(condition)
    if not c or index is None or index < 0:
        return None
    den = payout_denominator(condition)
    if not den:
        return None
    try:
        raw = _rpc('eth_call', [{'to': CTF, 'data': SEL_NUMERATOR + c + format(int(index), '064x')}, 'latest'])
        num = int(raw, 16) if isinstance(raw, str) and raw.startswith('0x') else None
    except (urllib.error.URLError, OSError, ValueError, TypeError):
        return None
    if num is None:
        return None
    return num / float(den)

def token_index(condition, token, fetch=None):
    c = str(condition or '')
    if not c.startswith('0x'):
        c = '0x' + c
    try:
        if fetch is not None:
            d = fetch(c)
        else:
            req = urllib.request.Request('https://clob.polymarket.com/markets/%s' % c, headers={'User-Agent': 'copybot-resolution'})
            with urllib.request.urlopen(req, timeout=15) as r:
                d = json.loads(r.read().decode())
    except Exception:
        return None
    for i, t in enumerate((d or {}).get('tokens') or []):
        if str(t.get('token_id')) == str(token):
            return i
    return None

def settled_payout(condition, token, fetch=None):
    idx = token_index(condition, token, fetch=fetch)
    if idx is None:
        return None
    pay = payout_for_index(condition, idx)
    if pay is None:
        return None
    if not MIN_PAYOUT <= pay <= MAX_PAYOUT:
        return None
    return pay
