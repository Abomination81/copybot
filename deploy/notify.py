import json
import os
import time
import urllib.error
import urllib.request
API = 'https://api.resend.com/emails'
TIMEOUT_SECS = 8
ALERT_TZ = os.environ.get('ALERT_TZ', 'America/Los_Angeles')

def local_stamp(t=None):
    t = time.time() if t is None else t
    utc = time.strftime('%H:%M:%SZ', time.gmtime(t))
    try:
        from zoneinfo import ZoneInfo
        import datetime
        dt = datetime.datetime.fromtimestamp(t, ZoneInfo(ALERT_TZ))
        return '%s (%s)' % (dt.strftime('%a %d %b %Y, %H:%M:%S %Z'), utc)
    except Exception:
        return '%s UTC (local zone %r unavailable)' % (time.strftime('%a %d %b %Y, %H:%M:%S', time.gmtime(t)), ALERT_TZ)

def _cfg():
    return (os.environ.get('RESEND_API_KEY') or '', os.environ.get('ALERT_EMAIL_TO') or '', os.environ.get('ALERT_EMAIL_FROM') or 'alerts@resend.dev')

def configured():
    key, to, _ = _cfg()
    return bool(key and to)

def why_not_configured():
    key, to, _ = _cfg()
    missing = [n for n, v in (('RESEND_API_KEY', key), ('ALERT_EMAIL_TO', to)) if not v]
    return 'missing %s' % ', '.join(missing) if missing else ''

def send(subject, text, log=print):
    key, to, frm = _cfg()
    if not (key and to):
        log('  ALERT NOT SENT (%s): %s' % (why_not_configured(), subject))
        return False
    body = json.dumps({'from': frm, 'to': [a.strip() for a in to.split(',') if a.strip()], 'subject': subject, 'text': text}).encode()
    req = urllib.request.Request(API, data=body, method='POST', headers={'Authorization': 'Bearer %s' % key, 'Content-Type': 'application/json', 'User-Agent': 'copybot-guardian/1.0'})
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT_SECS) as r:
            ok = 200 <= r.status < 300
            if not ok:
                log('  ALERT REJECTED (HTTP %s): %s' % (r.status, subject))
            return ok
    except urllib.error.HTTPError as e:
        detail = ''
        try:
            raw = e.read().decode()[:400]
        except Exception:
            raw = ''
        try:
            detail = str(json.loads(raw).get('message') or '')[:200]
        except Exception:
            detail = ' '.join(raw.split())[:200]
        log('  ALERT FAILED (HTTP %s%s): %s' % (e.code, ': ' + detail if detail else '', subject))
        return False
    except (urllib.error.URLError, OSError, ValueError) as e:
        log('  ALERT FAILED (%s): %s' % (type(e).__name__, subject))
        return False

def main():
    import argparse
    ap = argparse.ArgumentParser(description='send a test alert')
    ap.add_argument('--test', action='store_true')
    a = ap.parse_args()
    if not a.test:
        print('nothing to do; pass --test to send a test email')
        return 0
    if not configured():
        print('NOT CONFIGURED: %s' % why_not_configured())
        print('expected in the environment (systemd EnvironmentFile), never in the repo')
        return 2
    ok = send('Abomination81 Copybot: test alert', 'This is a test from deploy/notify.py.\n\nIf you are reading this, halt alerts can reach you.\nSent %s\n' % local_stamp())
    print('sent' if ok else 'FAILED — see the reason above')
    return 0 if ok else 1
if __name__ == '__main__':
    raise SystemExit(main())
