CONFIRMED = 'CONFIRMED'
REFUTED = 'REFUTED'
UNVERIFIABLE = 'UNVERIFIABLE'
LAGGING = 'LAGGING'
LAG_TOLERANCE = 0.9
MIN_SHARES = 5.0

class Ctx:

    def __init__(self, our_positions, leader_positions, position_value=None, our_shares=None):
        self.our_positions = our_positions
        self.leader_positions = leader_positions
        self.position_value = position_value
        self.our_shares = our_shares

def _exposure(fact, ctx):
    tok = fact.get('token')
    pct = fact.get('pct')
    allowed_ratio = fact.get('allowed_ratio')
    ours_events = fact.get('ours_shares')
    his_events = fact.get('his_shares')
    leader = fact.get('leader')
    if not tok or not leader or (not pct) or (not allowed_ratio):
        return (UNVERIFIABLE, 'the claim carries no token, leader or ratio to check')
    try:
        ours_events = float(ours_events)
        his_events = float(his_events)
    except (TypeError, ValueError):
        return (UNVERIFIABLE, 'the claim carries no comparable share counts')
    ours_book = ctx.our_positions()
    if ours_book is None:
        return (UNVERIFIABLE, 'our own venue positions could not be read')
    his_book = ctx.leader_positions(leader)
    if his_book is None:
        return (UNVERIFIABLE, "the leader's venue positions could not be read")
    key = str(tok)
    ours_venue = float(ours_book.get(key, 0.0) or 0.0)
    his_venue = float(his_book.get(key, 0.0) or 0.0)
    if his_venue <= MIN_SHARES:
        return (UNVERIFIABLE, 'the venue shows him holding %.1f sh of this token — too little to compare a ratio against' % his_venue)
    if ours_venue < ours_events * LAG_TOLERANCE:
        return (LAGGING, 'the venue shows us holding %.1f sh but our events already claim %.1f — the venue is behind, so it cannot speak to this' % (ours_venue, ours_events))
    if his_events > 0 and his_venue < his_events * LAG_TOLERANCE:
        return (UNVERIFIABLE, 'he held %.1f sh during the window and the venue now shows %.1f — he has reduced since, so the ratio is not comparable' % (his_events, his_venue))
    ratio = ours_venue / his_venue
    if ours_venue <= MIN_SHARES:
        return (REFUTED, 'the venue shows us holding only %.1f sh of this token — there is no runaway to stop' % ours_venue)
    if ratio <= allowed_ratio:
        return (REFUTED, 'the venue shows %.1f sh of ours against %.1f of his = %.1f%% copied, inside the %.1f%% allowance — no exposure breach exists' % (ours_venue, his_venue, ratio * 100.0, allowed_ratio * 100.0))
    return (CONFIRMED, 'the venue agrees: %.1f sh of ours against %.1f of his = %.1f%% copied, past the %.1f%% allowance' % (ours_venue, his_venue, ratio * 100.0, allowed_ratio * 100.0))
STRANDED_MIN_USD = 25.0

def _stranded(fact, ctx):
    tok = fact.get('token')
    if not tok:
        return (UNVERIFIABLE, 'the claim names no token')
    if getattr(ctx, 'our_shares', None) is not None:
        venue = ctx.our_shares(tok)
        if venue is None:
            return (UNVERIFIABLE, 'our own balance in this token could not be read — an unreadable balance is not an empty one')
        venue = float(venue)
        if venue < 0:
            return (UNVERIFIABLE, 'our balance came back negative (%.4f) — not a real answer' % venue)
    else:
        book = ctx.our_positions()
        if book is None:
            return (UNVERIFIABLE, 'our own venue positions could not be read')
        venue = float(book.get(str(tok), 0.0) or 0.0)
    if venue <= MIN_SHARES:
        return (REFUTED, 'the venue shows us holding %.2f sh of this token — there is nothing stranded to be halted over' % venue)
    if ctx.position_value is None:
        return (UNVERIFIABLE, 'no way to price the position we still hold')
    value = ctx.position_value(tok)
    if value is None:
        return (UNVERIFIABLE, 'we hold %.2f sh and it could not be priced — an unpriceable position is not a worthless one' % venue)
    if value < STRANDED_MIN_USD:
        return (REFUTED, 'we hold %.2f sh worth $%.4f — below the $%.2f that would justify stopping the pool' % (venue, value, STRANDED_MIN_USD))
    return (CONFIRMED, 'the venue agrees we are still holding %.2f sh worth $%.2f of a market he has left' % (venue, value))
CORROBORATORS = {'exposure': _exposure, 'stranded': _stranded}

def check(family, fact, ctx):
    fn = CORROBORATORS.get(family)
    if fn is None:
        return (CONFIRMED, 'no independent check exists for %r — halting as before' % family)
    try:
        verdict, evidence = fn(fact, ctx)
    except Exception as e:
        return (UNVERIFIABLE, 'the independent check itself failed (%r)' % (e,))
    if verdict not in (CONFIRMED, REFUTED, UNVERIFIABLE, LAGGING):
        return (UNVERIFIABLE, 'the independent check returned %r' % (verdict,))
    return (verdict, evidence)

def halts(verdict):
    return verdict != REFUTED
