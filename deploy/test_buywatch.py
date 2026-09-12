import json
import os
import sys
import tempfile
import time
import unittest
from unittest import mock
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import buywatch
import fillwatch
TOKEN = '12345678901234567890'

def lanes(leader='0xleader'):
    return {'example_lane_26': {'leader': leader, 'armed': True, 'ready': True, 'holdings': {}}}

def a_buy(token=TOKEN, shares=100.0, owner='0xleader', block=1000):
    return {'owner': owner, 'side': 0, 'token': token, 'shares': shares, 'block': block, 'tx': '0xdead'}

class PrenatalBuyTests(unittest.TestCase):

    def test_a_buy_before_the_lane_was_armed_is_not_UNSEEN(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, 'run'))
            with open(os.path.join(root, 'run', 'control.json.operator'), 'w') as f:
                json.dump({'lanes': {'newlane': {'armed': True, 'at': 5000}}}, f)
            with mock.patch.object(buywatch, 'BOT_DIR', root):
                since = buywatch.lane_armed_since()
            self.assertEqual(since['newlane'], 5000.0)

    def test_an_UNREADABLE_operator_file_suppresses_NOTHING(self):
        with tempfile.TemporaryDirectory() as root:
            with mock.patch.object(buywatch, 'BOT_DIR', root):
                self.assertEqual(buywatch.lane_armed_since(), {})

    def test_a_lane_with_no_stamp_is_never_suppressed(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, 'run'))
            with open(os.path.join(root, 'run', 'control.json.operator'), 'w') as f:
                json.dump({'lanes': {'old': {'armed': True}}}, f)
            with mock.patch.object(buywatch, 'BOT_DIR', root):
                self.assertNotIn('old', buywatch.lane_armed_since())

    def test_the_guard_does_not_hide_a_REAL_unseen_buy(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, 'run'))
            with open(os.path.join(root, 'run', 'control.json.operator'), 'w') as f:
                json.dump({'lanes': {'lane': {'armed': True, 'at': 1000}}}, f)
            with mock.patch.object(buywatch, 'BOT_DIR', root):
                since = buywatch.lane_armed_since()
            self.assertTrue(2000 >= since['lane'], 'a post-arm buy must still reach classify()')
            self.assertEqual(buywatch.classify({'t': 2000, 'token': 'T'}, 'lane', {}), 'UNSEEN')

class ClassifyTests(unittest.TestCase):

    def setUp(self):
        self.now = time.time()

    def dec(self, ev, why=None, dt=0.0, lane='example_lane_26', side='BUY', token=TOKEN):
        e = {'ev': ev, 'lane': lane, 'side': side, 'tok': token}
        if why:
            e['why'] = why
        return {(lane, token[:14]): [(self.now + dt, e)]}

    def test_a_copied_buy_is_FIRED(self):
        b = a_buy()
        b['t'] = self.now
        self.assertEqual(buywatch.classify(b, 'example_lane_26', self.dec('fire')), 'FIRED')

    def test_a_refused_buy_reports_WHY_and_is_not_an_alarm(self):
        b = a_buy()
        b['t'] = self.now
        v = buywatch.classify(b, 'example_lane_26', self.dec('signal_guard_skip', 'MarketCooldown { remaining_secs: 53761 }'))
        self.assertEqual(v, 'DECLINED:MarketCooldown', 'the remaining-seconds detail must be stripped so counts aggregate')

    def test_a_buy_with_NO_event_at_all_is_UNSEEN(self):
        b = a_buy()
        b['t'] = self.now
        self.assertEqual(buywatch.classify(b, 'example_lane_26', {}), 'UNSEEN')

    def test_an_event_on_a_DIFFERENT_token_does_not_excuse_this_buy(self):
        b = a_buy()
        b['t'] = self.now
        other = self.dec('fire', token='99999999999999999')
        self.assertEqual(buywatch.classify(b, 'example_lane_26', other), 'UNSEEN')

    def test_an_event_from_a_DIFFERENT_lane_does_not_excuse_this_buy(self):
        b = a_buy()
        b['t'] = self.now
        self.assertEqual(buywatch.classify(b, 'example_lane_26', self.dec('fire', lane='example_lane_25')), 'UNSEEN')

    def test_an_event_FAR_outside_the_join_window_does_not_count(self):
        b = a_buy()
        b['t'] = self.now
        far = self.dec('fire', dt=buywatch.JOIN_SECS + 60)
        self.assertEqual(buywatch.classify(b, 'example_lane_26', far), 'UNSEEN')

    def test_a_SELL_fire_never_counts_as_copying_a_BUY(self):
        b = a_buy()
        b['t'] = self.now
        self.assertEqual(buywatch.classify(b, 'example_lane_26', self.dec('fire', side='SELL')), 'UNSEEN')

class WindowTests(unittest.TestCase):

    def test_the_window_is_LONGER_than_the_timer_interval(self):
        self.assertGreater(buywatch.WINDOW_SECS, 900, 'a 15-minute timer needs a window longer than 15 minutes')

    def test_the_window_is_measured_in_MEASURED_blocks(self):
        fillwatch.BLOCK_SECS = None
        try:
            fillwatch.rpc = lambda m, p: hex(90000000) if m == 'eth_blockNumber' else {'timestamp': hex(int(1000000 + int(p[0], 16) * 1.5))}
            blk = fillwatch.measure_block_secs()
            self.assertAlmostEqual(blk, 1.5, places=3)
            self.assertEqual(int(buywatch.WINDOW_SECS / blk), 840, '1260s at 1.5s/block is 840 blocks')
        finally:
            fillwatch.BLOCK_SECS = None

class RunTests(unittest.TestCase):

    def setUp(self):
        fillwatch.BLOCK_SECS = None
        self.tmp = '/tmp/buywatch_test_%d' % os.getpid()
        os.makedirs(os.path.join(self.tmp, 'run'), exist_ok=True)
        os.makedirs(os.path.join(self.tmp, 'data'), exist_ok=True)
        self._dir = buywatch.BOT_DIR
        buywatch.BOT_DIR = self.tmp
        buywatch.ALERT_PATH = os.path.join(self.tmp, 'run', 'buywatch_alerts.jsonl')

    def tearDown(self):
        fillwatch.BLOCK_SECS = None
        buywatch.BOT_DIR = self._dir
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _rpc(self, probe_empty=False):

        def fake(method, params):
            if method == 'eth_blockNumber':
                return hex(90000000)
            if method == 'eth_getBlockByNumber':
                return {'timestamp': hex(int(1000000 + int(params[0], 16) * 1.5))}
            if method == 'eth_getLogs':
                return [] if probe_empty else [{'x': 1}]
            return []
        return fake

    def test_a_leader_buy_with_no_decision_is_NOT_PASS_and_writes_an_alert(self):
        buys = [a_buy()]
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'rpc', side_effect=self._rpc()), mock.patch.object(fillwatch, 'fills_in_range', return_value=[{'e': 1}]), mock.patch.object(fillwatch, 'decode_fill', return_value=buys):
            rc = buywatch.run(1260, dry_run=False)
        self.assertEqual(rc, 1)
        with open(buywatch.ALERT_PATH) as f:
            row = json.loads(f.read().strip())
        self.assertEqual(row['kind'], 'unseen_leader_buys')
        self.assertEqual(row['count'], 1)

    def test_a_QUIET_window_passes_without_touching_the_alert_file(self):
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'rpc', side_effect=self._rpc()), mock.patch.object(fillwatch, 'fills_in_range', return_value=[]), mock.patch.object(fillwatch, 'decode_fill', return_value=[]):
            self.assertEqual(buywatch.run(1260, dry_run=False), 0)
        self.assertFalse(os.path.exists(buywatch.ALERT_PATH))

    def test_a_filter_that_matches_NOTHING_is_UNVERIFIED_never_a_pass(self):
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'rpc', side_effect=self._rpc(probe_empty=True)):
            self.assertEqual(buywatch.run(1260, dry_run=False), 3)

    def test_a_SELL_by_the_leader_is_not_this_tool_s_business(self):
        sells = [dict(a_buy(), side=1)]
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'rpc', side_effect=self._rpc()), mock.patch.object(fillwatch, 'fills_in_range', return_value=[{'e': 1}]), mock.patch.object(fillwatch, 'decode_fill', return_value=sells):
            self.assertEqual(buywatch.run(1260, dry_run=False), 0)

    def test_it_NEVER_halts_a_lane(self):
        buys = [a_buy()]
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'rpc', side_effect=self._rpc()), mock.patch.object(fillwatch, 'fills_in_range', return_value=[{'e': 1}]), mock.patch.object(fillwatch, 'decode_fill', return_value=buys), mock.patch.object(fillwatch, 'halt_buys') as halt:
            buywatch.run(1260, dry_run=False)
        halt.assert_not_called()

    def test_a_broken_chain_read_is_UNVERIFIED_not_a_crash(self):

        def boom(*_a, **_k):
            raise fillwatch.Unverifiable('rpc down')
        with mock.patch.object(fillwatch, 'bot_state', return_value=lanes()), mock.patch.object(fillwatch, 'measure_block_secs', side_effect=boom):
            self.assertEqual(buywatch.main.__wrapped__(1260, False) if hasattr(buywatch.main, '__wrapped__') else _main_rc(), 3)

def _main_rc():
    with mock.patch.object(sys, 'argv', ['buywatch.py']):
        return buywatch.main()
if __name__ == '__main__':
    unittest.main()
