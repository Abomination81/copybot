PASS = 'PASS'
FAIL = 'FAIL'
UNKNOWN = 'UNKNOWN'
MIN_SAMPLES = 5

class Sensor:

    def __init__(self, name, alarm, why, source=None, requires=(), intersects=None, forbids=()):
        self.name = name
        self.alarm = alarm
        self.why = why
        self.source = source
        self.requires = tuple(requires)
        self.intersects = intersects
        self.forbids = tuple(forbids)
SENSORS = [Sensor('exposure_fact', 'guardian runaway/exposure', 'the corroborated exposure claim needs the token and both share counts; without them every claim is UNVERIFIABLE and halts on its own evidence', source='events:fire', requires=('tok', 'shares', 'his_fill', 'his_order')), Sensor('reject_valuation', 'watcher REJECT severity', 'a rejected sell is judged on what the position is WORTH, looked up by (lane, token) — the event carries no share count, which is exactly why the first version of this rule could never fire', source='events:clob_resp', requires=('lane', 'tok', 'side', 'paths'), forbids=('shares',)), Sensor('lane_leader_map', 'guardian corroboration', 'the leader address comes from /api/pool; /api/status lanes do NOT carry it, and reading it there makes every corroboration a silent no-op', source='api:/api/pool:lanes', requires=('name', 'leader')), Sensor('position_marks', 'watcher dust valuation + dashboard drag', 'valuing a position needs a lane, a token, a mark and a share count', source='api:/api/positions:positions', requires=('lane', 'token', 'mark', 'shares')), Sensor('miss_suppression_domain', 'watcher halt-aware MISS', 'the lane a leader fill belongs to must be drawn from the SAME namespace as the halted-lane set, or the suppression can never match and a halt manufactures MISSes from its own consequences', intersects=('watcher_fill_lanes', 'operator_lane_names'))]

def _check_fields(records, sensor):
    if records is None:
        return (UNKNOWN, 'the source could not be read')
    if len(records) < MIN_SAMPLES:
        return (UNKNOWN, 'only %d live sample(s); absence proves nothing yet' % len(records))
    missing = [k for k in sensor.requires if not any((isinstance(r, dict) and k in r for r in records))]
    if missing:
        return (FAIL, '%d live sample(s) and NOT ONE carries %s — this alarm cannot fire' % (len(records), ', '.join(missing)))
    present = [k for k in sensor.forbids if any((isinstance(r, dict) and k in r for r in records))]
    if present:
        return (PASS, 'all required keys present; note %s now appears, which it did not when this rule was written' % ', '.join(present))
    return (PASS, 'all %d required key(s) observed across %d sample(s)' % (len(sensor.requires), len(records)))

def _check_domains(sets, sensor):
    left_name, right_name = sensor.intersects
    left, right = (sets.get(left_name), sets.get(right_name))
    if left is None or right is None:
        return (UNKNOWN, 'one side could not be read')
    left, right = (set(left), set(right))
    if not left or not right:
        return (UNKNOWN, '%s=%d value(s), %s=%d — nothing to compare yet' % (left_name, len(left), right_name, len(right)))
    both = left & right
    if not both:
        return (FAIL, '%s and %s share NO values (%s vs %s) — every comparison between them is dead' % (left_name, right_name, sorted(left)[:3], sorted(right)[:3]))
    return (PASS, '%d shared value(s), e.g. %r' % (len(both), sorted(both)[0]))

def run(records_for, value_sets=None):
    out = []
    value_sets = value_sets or {}
    for s in SENSORS:
        try:
            if s.intersects:
                status, detail = _check_domains(value_sets, s)
            else:
                status, detail = _check_fields(records_for(s.source), s)
        except Exception as e:
            status, detail = (UNKNOWN, 'the check itself failed: %r' % (e,))
        out.append((s, status, detail))
    return out

def failures(results):
    return [(s, d) for s, st, d in results if st == FAIL]

def summary(results):
    n = {PASS: 0, FAIL: 0, UNKNOWN: 0}
    for _s, st, _d in results:
        n[st] = n.get(st, 0) + 1
    return '%d passed, %d FAILED, %d unknown' % (n[PASS], n[FAIL], n[UNKNOWN])
