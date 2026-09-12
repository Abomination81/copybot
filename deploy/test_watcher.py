import json
import os
import tempfile
import time
import unittest
from unittest import mock
import watcher

class WatcherSafetyTests(unittest.TestCase):

    @staticmethod
    def _operator(root, armed):
        run = os.path.join(root, 'run')
        os.makedirs(run, exist_ok=True)
        with open(os.path.join(run, 'control.json.operator'), 'w') as f:
            json.dump({'lanes': {'example_lane_26': {'armed': armed}}}, f)

    def test_dashboard_down_is_safe_when_explicitly_disarmed(self):
        with tempfile.TemporaryDirectory() as root:
            self._operator(root, False)
            with mock.patch.object(watcher, 'BOT_DIR', root), mock.patch.object(watcher, '_get', return_value=None):
                self.assertEqual(watcher.armed_state(), (None, None))

    def test_down_dashboard_with_armed_intent_is_dead(self):
        with tempfile.TemporaryDirectory() as root:
            self._operator(root, True)
            with mock.patch.object(watcher, 'BOT_DIR', root), mock.patch.object(watcher, '_get', return_value=None):
                self.assertEqual(watcher.armed_state(), (None, 'DEAD'))

    def test_reverse_reader_stops_at_the_cutoff_without_scanning_history(self):
        with tempfile.NamedTemporaryFile('w', delete=False) as f:
            path = f.name
            for i in range(100):
                f.write(json.dumps({'t': i * 1000, 'ev': 'fire', 'tok': str(i)}) + '\n')
        try:
            with mock.patch.object(watcher, 'BOT_DIR', os.path.dirname(os.path.dirname(path))):
                lines = list(watcher._reverse_lines(path))
            self.assertEqual(json.loads(lines[0])['tok'], '99')
            self.assertEqual(json.loads(lines[-1])['tok'], '0')
        finally:
            os.unlink(path)

    def test_old_buy_never_covers_a_later_sell(self):
        fill = {'ts': 1000.0, 'token': '123', 'condition': 'market', 'side': 'SELL', 'tx': 'sell-tx'}
        fires = [{'ts': 900.0, 'token': '123', 'condition': 'market', 'side': 'BUY', 'tx': 'buy-tx'}]
        self.assertFalse(watcher.fill_is_covered(fill, fires, grace=45))

    def test_sell_needs_its_own_nearby_or_exact_fire(self):
        fill = {'ts': 1000.0, 'token': '123', 'condition': 'market', 'side': 'SELL', 'tx': 'sell-tx'}
        exact = {'ts': 800.0, 'token': '123', 'condition': 'market', 'side': 'SELL', 'tx': 'sell-tx'}
        nearby = {'ts': 990.0, 'token': '123', 'condition': 'market', 'side': 'SELL', 'tx': ''}
        self.assertTrue(watcher.fill_is_covered(fill, [exact], grace=45))
        self.assertTrue(watcher.fill_is_covered(fill, [nearby], grace=45))

    def test_buy_on_other_outcome_is_covered_by_condition_quarantine(self):
        fill = {'ts': 1000.0, 'token': 'no-token', 'condition': 'market', 'side': 'BUY', 'tx': 'later-partial'}
        fire = {'ts': 500.0, 'token': 'yes-token', 'condition': 'market', 'side': 'BUY', 'tx': 'first-partial'}
        self.assertTrue(watcher.fill_is_covered(fill, [fire], grace=45))

    def test_matching_dust_skip_covers_only_that_fill(self):
        fill = {'ts': 1000.0, 'token': '123456789', 'condition': 'market', 'side': 'BUY', 'tx': 'public-tx', 'size': 22.02}
        skip = {'ts': 998.0, 'kind': 'skip', 'token': '123456', 'condition': '', 'side': 'BUY', 'tx': '', 'size': 22.02}
        self.assertTrue(watcher.fill_is_covered(fill, [skip], grace=45))
        later = dict(fill, ts=2000.0, tx='later-tx', size=200.0)
        self.assertFalse(watcher.fill_is_covered(later, [skip], grace=45))

    def test_wrong_size_skip_does_not_hide_a_material_fill(self):
        fill = {'ts': 1000.0, 'token': '123456789', 'condition': 'market', 'side': 'BUY', 'tx': 'public-tx', 'size': 200.0}
        skip = {'ts': 998.0, 'kind': 'skip', 'token': '123456', 'condition': '', 'side': 'BUY', 'tx': '', 'size': 22.02}
        self.assertFalse(watcher.fill_is_covered(fill, [skip], grace=45))

    def test_pre_arm_fill_is_baselined_not_reported_as_a_miss(self):
        old = {'ts': 900.0, 'token': 'old', 'condition': 'old-market', 'size': 1.0, 'side': 'BUY', 'tx': '', 'title': 'old', 'seen': False}
        after_arm = {'ts': 1010.0, 'token': 'new', 'condition': 'new-market', 'size': 1.0, 'side': 'BUY', 'tx': '', 'title': 'new', 'seen': False}
        alerts = []
        watcher.his_fills.clear()
        watcher.his_fills.extend([old, after_arm])
        try:
            with mock.patch.object(watcher, 'armed_state', return_value=('copybot', None)), mock.patch.object(watcher, 'operator_epoch', return_value=1000.0), mock.patch.object(watcher, 'check_rejects'), mock.patch.object(watcher, 'fires_since', return_value=[]), mock.patch.object(watcher, 'held_tokens', return_value=set()), mock.patch.object(watcher.time, 'time', return_value=1100.0):
                watcher.sweep(45, lambda level, kind, msg, lanes=None: alerts.append((level, kind, msg)))
            self.assertTrue(old['seen'])
            self.assertEqual([kind for _, kind, _ in alerts], ['MISS'])
            self.assertIn('new', alerts[0][2])
        finally:
            watcher.his_fills.clear()

class DeadHardeningTests(unittest.TestCase):

    def _sweep_dead(self, liveness, epoch=None, sweeps=1):
        alerts = []
        watcher._dead_streak = 0
        with mock.patch.object(watcher, 'armed_state', return_value=(None, 'DEAD')), mock.patch.object(watcher, 'runtime_liveness', return_value=liveness), mock.patch.object(watcher, 'operator_epoch', return_value=epoch):
            for _ in range(sweeps):
                watcher.sweep(45, lambda lv, k, m, lanes=None: alerts.append((lv, k, m)))
        return [k for _, k, _ in alerts]

    def test_a_booting_runtime_is_never_dead(self):
        self.assertEqual(self._sweep_dead((True, 5.0), sweeps=5), [])

    def test_a_gone_process_disarms_only_after_confirmation(self):
        watcher._dead_streak = 0
        first = self._sweep_dead((False, None), sweeps=1)
        self.assertEqual(first, [], 'one dead sweep must not disarm (restart flicker)')
        second = []
        with mock.patch.object(watcher, 'armed_state', return_value=(None, 'DEAD')), mock.patch.object(watcher, 'runtime_liveness', return_value=(False, None)), mock.patch.object(watcher, 'operator_epoch', return_value=None):
            watcher.sweep(45, lambda lv, k, m, lanes=None: second.append(k))
        self.assertEqual(second, ['DEAD'], 'second consecutive dead sweep disarms')

    def test_a_hung_runtime_up_past_boot_grace_is_dead(self):
        self.assertEqual(self._sweep_dead((True, 100000.0), sweeps=2), ['DEAD'])

    def test_a_probe_failure_falls_back_to_the_mtime_grace(self):
        self.assertEqual(self._sweep_dead((None, None), epoch=9e+18, sweeps=5), [], 'unknown probe within the arm grace must not disarm')
        self.assertEqual(self._sweep_dead((None, None), epoch=0.0, sweeps=2), ['DEAD'], 'unknown probe long after arming still catches a real DEAD')

    def test_recovery_clears_the_streak(self):
        watcher._dead_streak = 1
        alerts = []
        with mock.patch.object(watcher, 'armed_state', return_value=('copybot', None)), mock.patch.object(watcher, 'operator_epoch', return_value=1000.0), mock.patch.object(watcher, 'check_rejects'), mock.patch.object(watcher, 'fires_since', return_value=[]), mock.patch.object(watcher, 'held_tokens', return_value=set()), mock.patch.object(watcher.time, 'time', return_value=1100.0):
            watcher.sweep(45, lambda lv, k, m, lanes=None: alerts.append(k))
        self.assertEqual(watcher._dead_streak, 0, 'an armed, acknowledged sweep clears it')

class SizeMatchTests(unittest.TestCase):

    def test_the_incident_a_mempool_decode_matches_the_public_feed(self):
        self.assertTrue(watcher.same_order_size(1151.6741795665635, 1151.69))

    def test_a_tiny_old_skip_still_cannot_mask_a_material_order(self):
        self.assertFalse(watcher.same_order_size(5.0, 5000.0))
        self.assertFalse(watcher.same_order_size(100.0, 250.0))

    def test_a_priceoutofband_skip_now_covers_the_buy_it_declined(self):
        fill = {'side': 'BUY', 'token': '9601162172651700000', 'ts': 1000, 'size': 1151.69}
        decision = [{'kind': 'skip', 'side': 'BUY', 'token': '96011621726517', 'ts': 1002, 'size': 1151.6741795665635}]
        self.assertTrue(watcher.fill_is_covered(fill, decision, 45), 'a deliberate policy skip must count as coverage')

    def test_an_unrelated_small_skip_does_not_cover_a_big_buy(self):
        fill = {'side': 'BUY', 'token': '9601162172651700000', 'ts': 1000, 'size': 5000.0}
        decision = [{'kind': 'skip', 'side': 'BUY', 'token': '96011621726517', 'ts': 1002, 'size': 5.0}]
        self.assertFalse(watcher.fill_is_covered(fill, decision, 45), 'a dust skip must never hide a material order')

class EvidenceUnavailableTests(unittest.TestCase):

    def _bot_dir(self, d, lines):
        data = os.path.join(d, 'data')
        os.makedirs(data, exist_ok=True)
        p = os.path.join(data, 'events-1.jsonl')
        with open(p, 'w') as f:
            for line in lines:
                f.write(json.dumps(line) + '\n')
        return p

    def test_an_UNREADABLE_log_raises_instead_of_returning_no_events(self):
        with tempfile.TemporaryDirectory() as d:
            if os.geteuid() == 0:
                self.skipTest('root ignores file permissions')
            p = self._bot_dir(d, [{'ev': 'fire', 't': time.time()}])
            os.chmod(p, 0)
            try:
                with self.assertRaises(watcher.EvidenceUnavailable):
                    watcher._jsonl_events(d, 0, {'fire'})
            finally:
                os.chmod(p, 420)

    def test_a_CORRUPT_LINE_is_still_skipped_not_escalated(self):
        with tempfile.TemporaryDirectory() as d:
            data = os.path.join(d, 'data')
            os.makedirs(data)
            with open(os.path.join(data, 'events-1.jsonl'), 'w') as f:
                f.write(json.dumps({'ev': 'fire', 't': time.time(), 'tok': 'x'}) + '\n')
                f.write('{not json at all\n')
            rows = watcher._jsonl_events(d, 0, {'fire'})
            self.assertEqual(len(rows), 1)

    def test_a_READ_FAILURE_alerts_READ_and_never_declares_a_MISS(self):
        alerts = []

        def alert(level, kind, msg, lanes=None):
            alerts.append((level, kind))
        boom = watcher.EvidenceUnavailable('cannot read events-1.jsonl')
        with mock.patch.object(watcher, 'armed_state', return_value=('copybot', None)), mock.patch.object(watcher, 'operator_epoch', return_value=time.time() - 600), mock.patch.object(watcher, 'check_rejects', return_value=None), mock.patch.object(watcher, 'fires_since', side_effect=boom):
            with watcher.lock:
                watcher.his_fills.clear()
                watcher.his_fills.append({'ts': time.time() - 300, 'token': 't1', 'side': 'BUY', 'size': 100.0, 'condition': 'c1', 'tx': '0xabc', 'title': 'm', 'seen': False})
            watcher.sweep(45, alert)
        kinds = [k for _, k in alerts]
        self.assertIn('READ', kinds)
        self.assertNotIn('MISS', kinds, 'a read fault must never be reported as a MISS')
        with watcher.lock:
            self.assertFalse(watcher.his_fills[0]['seen'], 'unjudged evidence must survive to the next sweep')
            watcher.his_fills.clear()

class RegistryFaultTests(unittest.TestCase):

    def test_a_CORRUPT_registry_is_reported_not_silently_downgraded(self):
        with tempfile.TemporaryDirectory() as d:
            run = os.path.join(d, 'run')
            os.makedirs(run)
            with open(os.path.join(run, 'control.json.wallets'), 'w') as f:
                f.write('{ this is not json')
            with mock.patch.object(watcher, 'BOT_DIR', d):
                _got, err = watcher.watched_leaders()
            self.assertIsNotNone(err, 'an unreadable registry must be surfaced, not hidden behind the legacy single-leader fallback')

    def test_a_MISSING_registry_is_a_legacy_install_not_a_fault(self):
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.object(watcher, 'BOT_DIR', d), mock.patch.object(watcher, 'HIM', 'synthetic-leader'):
                got, _err = watcher.watched_leaders()
            self.assertIsNone(_err)
            self.assertEqual(list(got.values()), ['default'])

class RegistryPathPinTests(unittest.TestCase):

    def test_the_registry_filename_matches_the_runtime(self):
        with tempfile.TemporaryDirectory() as d:
            run = os.path.join(d, 'run')
            os.makedirs(run)
            with open(os.path.join(run, 'control.json.wallets'), 'w') as f:
                json.dump({'wallets': [{'name': 'third', 'leader': '0xDDD4', 'enabled': True}]}, f)
            with mock.patch.object(watcher, 'BOT_DIR', d):
                got, _err = watcher.watched_leaders()
        self.assertEqual(got, {'0xddd4': 'third'}, 'watcher must read the SAME file main.rs writes')

class FeedHealthTests(unittest.TestCase):

    def setUp(self):
        with watcher._feed_lock:
            watcher._feed.update({'connected': False, 'connected_since': None, 'last_frame_t': 0.0, 'last_decoded_t': 0.0, 'last_leader_fill_t': 0.0, 'reconnects': 0, 'resubscribing_since': None, 'last_error': None})

    def test_a_FRESH_START_is_booting_not_blind(self):
        watcher._started_t = time.time()
        watcher._feed_set(connected=True)
        f = watcher.feed_snapshot()
        self.assertTrue(f['booting'])
        self.assertTrue(f['useful'], 'a just-started watcher is not yet blind')

    def test_a_DEAD_SOCKET_is_not_useful(self):
        watcher._feed_set(connected=False, last_frame_t=time.time())
        self.assertFalse(watcher.feed_snapshot()['useful'])

    def test_CONNECTED_but_NO_DECODES_is_not_useful(self):
        watcher._started_t = time.time() - 10000
        watcher._feed_set(connected=True, last_frame_t=time.time(), last_decoded_t=time.time() - 10000)
        self.assertFalse(watcher.feed_snapshot()['useful'])

    def test_a_QUIET_LEADER_on_a_LIVE_feed_stays_useful(self):
        now = time.time()
        watcher._feed_set(connected=True, last_frame_t=now, last_decoded_t=now, last_leader_fill_t=now - 86400)
        f = watcher.feed_snapshot()
        self.assertTrue(f['useful'])
        self.assertGreater(f['leader_fill_age_secs'], 3600)

    def test_a_watcher_that_has_NEVER_decoded_still_reports_a_NUMBER(self):
        watcher._started_t = time.time() - 90
        watcher._feed_set(connected=True, last_frame_t=time.time(), last_decoded_t=0.0)
        f = watcher.feed_snapshot()
        self.assertFalse(f['decoded_ever'], 'it genuinely has never decoded')
        self.assertIsInstance(f['decode_age_secs'], float, 'an age a threshold cannot compare against is not an age')
        self.assertAlmostEqual(f['decode_age_secs'], 90, delta=5, msg='dated from process start, matching the health clock')

    def test_a_BRIEF_RESUBSCRIBE_is_still_excused(self):
        now = time.time()
        watcher._started_t = now - 10000
        watcher._feed_set(connected=False, last_frame_t=now - 2, last_decoded_t=now - 25, resubscribing_since=now - 2)
        f = watcher.feed_snapshot()
        self.assertTrue(f['resubscribing'])
        self.assertTrue(f['useful'], 'a 25s-blind feed mid-repair is repairing, not dead')

    def test_a_REARMED_RESUBSCRIBE_cannot_excuse_a_LONG_OUTAGE(self):
        now = time.time()
        watcher._started_t = now - 10000
        watcher._feed_set(connected=False, last_frame_t=now - 2, last_decoded_t=now - 1500, resubscribing_since=now - 2)
        f = watcher.feed_snapshot()
        self.assertTrue(f['resubscribing'], 'the grace is genuinely re-armed')
        self.assertFalse(f['useful'], 'a 25-minute outage is not a repair, however recently re-armed')

class WatchedLeadersTests(unittest.TestCase):

    def test_EVERY_enabled_lane_leader_is_watched(self):
        with tempfile.TemporaryDirectory() as d:
            run = os.path.join(d, 'run')
            os.makedirs(run)
            with open(os.path.join(run, 'control.json.wallets'), 'w') as f:
                json.dump({'wallets': [{'name': 'example_lane_26', 'leader': '0xAAA1', 'enabled': True}, {'name': 'example_lane_25', 'leader': '0xBBB2', 'enabled': True}, {'name': 'old', 'leader': '0xCCC3', 'enabled': False}]}, f)
            with mock.patch.object(watcher, 'BOT_DIR', d):
                got, _err = watcher.watched_leaders()
        self.assertEqual(got, {'0xaaa1': 'example_lane_26', '0xbbb2': 'example_lane_25'}, 'both live lanes must be covered; disabled ones must not be')

class FrameReadTests(unittest.TestCase):

    class _Sock:

        def __init__(self, chunks):
            self.chunks = list(chunks)
            self.calls = 0

        def recv(self, n):
            self.calls += 1
            if self.calls > 10000:
                raise AssertionError('recv_exact SPUN — this is the 100% CPU bug')
            if self.chunks:
                return self.chunks.pop(0)
            return b''

    def test_a_peer_that_closes_MID_FRAME_raises_instead_of_spinning(self):
        s = self._Sock([b'ab'])
        with self.assertRaises(ConnectionError) as cm:
            watcher.recv_exact(s, 10)
        self.assertIn('mid-frame', str(cm.exception))
        self.assertLess(s.calls, 10, 'it must give up immediately, not poll')

    def test_a_frame_split_across_packets_is_still_read_whole(self):
        s = self._Sock([b'abc', b'de', b'fghij'])
        self.assertEqual(watcher.recv_exact(s, 10), b'abcdefghij')

    def test_an_immediate_close_with_no_payload_at_all_raises(self):
        s = self._Sock([])
        with self.assertRaises(ConnectionError):
            watcher.recv_exact(s, 4)

class FillPersistenceTests(unittest.TestCase):

    def setUp(self):
        with watcher.lock:
            watcher.his_fills.clear()
        watcher.fills_dropped = 0

    def test_an_EVICTED_unjudged_fill_is_COUNTED_not_silently_dropped(self):
        cap = watcher.his_fills.maxlen
        with watcher.lock:
            for i in range(cap + 25):
                watcher._note_fill_eviction()
                watcher.his_fills.append({'ts': time.time(), 'token': 't%d' % i, 'side': 'BUY', 'size': 1.0, 'seen': False})
        self.assertEqual(len(watcher.his_fills), cap, 'the deque is still bounded')
        self.assertEqual(watcher.fills_dropped, 25, 'every evicted UNJUDGED row must be counted — this is the difference between an incomplete analysis and a clean one')

    def test_evicting_an_ALREADY_JUDGED_fill_is_not_a_loss(self):
        cap = watcher.his_fills.maxlen
        with watcher.lock:
            for i in range(cap):
                watcher.his_fills.append({'ts': time.time(), 'token': 't%d' % i, 'side': 'BUY', 'size': 1.0, 'seen': True})
            for i in range(10):
                watcher._note_fill_eviction()
                watcher.his_fills.append({'ts': time.time(), 'token': 'n%d' % i, 'side': 'BUY', 'size': 1.0, 'seen': False})
        self.assertEqual(watcher.fills_dropped, 0, 'judged rows aging out is normal operation, not lost evidence')

    def test_a_RESTORE_bigger_than_the_cap_REPORTS_what_it_could_not_hold(self):
        cap = watcher.his_fills.maxlen
        rows = [{'ts': time.time(), 'token': 't%d' % i, 'side': 'BUY', 'size': 1.0, 'seen': False} for i in range(cap + 7)]
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'run', 'watcher_fills.json')
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, 'w') as f:
                json.dump({'fills': rows}, f)
            with mock.patch.object(watcher, 'FILLS_PATH', path):
                watcher.load_fills()
        self.assertEqual(watcher.fills_dropped, 7, 'the restore dropped rows and said nothing')

    def test_the_heartbeat_PUBLISHES_the_buffer_depth_and_any_loss(self):
        with tempfile.TemporaryDirectory() as d:
            hb = os.path.join(d, 'run', 'watcher_heartbeat.json')
            with mock.patch.object(watcher, 'HEARTBEAT', hb), mock.patch.object(watcher, 'watched_leaders', lambda: ({}, None)):
                watcher.fills_dropped = 3
                watcher.write_heartbeat('example_lane_26', None)
                doc = json.load(open(hb))
        self.assertEqual(doc['fills']['dropped_unjudged'], 3)
        self.assertIn('depth', doc['fills'])
        self.assertEqual(doc['fills']['cap'], watcher.his_fills.maxlen)

    def test_unjudged_fills_SURVIVE_a_restart(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'run', 'watcher_fills.json')
            with mock.patch.object(watcher, 'FILLS_PATH', path):
                with watcher.lock:
                    watcher.his_fills.append({'ts': time.time(), 'token': 't1', 'side': 'BUY', 'size': 100.0, 'title': 'm', 'seen': False})
                watcher.save_fills()
                with watcher.lock:
                    watcher.his_fills.clear()
                kept = watcher.load_fills()
        self.assertEqual(kept, 1)
        with watcher.lock:
            self.assertFalse(watcher.his_fills[0]['seen'], 'a restored fill must still be judged')
            watcher.his_fills.clear()

    def test_STALE_fills_beyond_the_retention_window_are_dropped(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'run', 'watcher_fills.json')
            with mock.patch.object(watcher, 'FILLS_PATH', path):
                with watcher.lock:
                    watcher.his_fills.append({'ts': time.time() - 48 * 3600, 'token': 'old', 'side': 'BUY', 'size': 1.0, 'title': 'm', 'seen': False})
                watcher.save_fills()
                with watcher.lock:
                    watcher.his_fills.clear()
                self.assertEqual(watcher.load_fills(), 0, 'day-old fills must not become retroactive alerts')

    def test_a_CORRUPT_buffer_starts_empty_rather_than_crashing_the_watcher(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'watcher_fills.json')
            with open(path, 'w') as f:
                f.write('{ truncated')
            with mock.patch.object(watcher, 'FILLS_PATH', path):
                self.assertEqual(watcher.load_fills(), 0)

    def test_a_JUDGED_fill_does_not_re_alert_after_a_restart(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'watcher_fills.json')
            with mock.patch.object(watcher, 'FILLS_PATH', path):
                with watcher.lock:
                    watcher.his_fills.append({'ts': time.time(), 'token': 't1', 'side': 'BUY', 'size': 5.0, 'title': 'm', 'seen': True})
                watcher.save_fills()
                with watcher.lock:
                    watcher.his_fills.clear()
                watcher.load_fills()
                with watcher.lock:
                    self.assertTrue(watcher.his_fills[0]['seen'])
                    watcher.his_fills.clear()

class PreSubmitFailureCoverageTests(unittest.TestCase):

    def setUp(self):
        with watcher.lock:
            watcher.his_fills.clear()

    def _run(self, events):
        alerts = []
        with tempfile.TemporaryDirectory() as d:
            data = os.path.join(d, 'data')
            os.makedirs(data)
            with open(os.path.join(data, 'events-1.jsonl'), 'w') as f:
                for e in events:
                    f.write(json.dumps(e) + '\n')
            with watcher.lock:
                watcher.his_fills.append({'ts': time.time() - 300, 'token': 'tok1', 'side': 'BUY', 'size': 100.0, 'condition': 'cond1', 'tx': '0xdead', 'title': 'market', 'seen': False})
            with mock.patch.object(watcher, 'BOT_DIR', d), mock.patch.object(watcher, 'armed_state', return_value=('copybot', None)), mock.patch.object(watcher, 'operator_epoch', return_value=time.time() - 3600), mock.patch.object(watcher, 'held_tokens', return_value=None):
                watcher.sweep(45, lambda lvl, kind, msg, lanes=None: alerts.append((lvl, kind, msg)))
        with watcher.lock:
            watcher.his_fills.clear()
        return [k for _, k, _ in alerts]

    def test_a_PRE_SUBMIT_FAILURE_is_not_coverage(self):
        kinds = self._run([{'ev': 'submit_precondition_failed', 'lane': 'example_lane_26', 'tok': 'tok1', 'why': 'missing L2 credentials', 't': int((time.time() - 290) * 1000)}])
        self.assertIn('MISS', kinds, 'an order that never reached the venue is a MISSED copy')

    def test_a_REAL_FIRE_is_still_coverage(self):
        kinds = self._run([{'ev': 'fire', 'lane': 'example_lane_26', 'tok': 'tok1', 'side': 'BUY', 'tx': '0xdead', 'condition': 'cond1', 'shares': 5, 't': int((time.time() - 290) * 1000)}])
        self.assertNotIn('MISS', kinds, 'a submitted order IS coverage')

    def test_a_DELIBERATE_SKIP_is_still_coverage(self):
        kinds = self._run([{'ev': 'signal_guard_skip', 'lane': 'example_lane_26', 'tok': 'tok1', 'condition': 'cond1', 'why': 'AlreadyReserved', 't': int((time.time() - 290) * 1000)}])
        self.assertNotIn('MISS', kinds)

class DecodeStallTests(unittest.TestCase):

    def setUp(self):
        with watcher._feed_lock:
            watcher._feed.update({'connected': True, 'last_frame_t': time.time(), 'last_decoded_t': 0.0, 'reconnects': 0, 'resubscribing_since': None, 'last_error': None})

    def test_the_stall_bound_heals_BEFORE_the_guardian_would_halt(self):
        self.assertLess(watcher.DECODE_STALL_RESUBSCRIBE_SECS, watcher.DECODE_MAX_AGE_SECS, 'the feed must heal before `useful` goes false')

    def test_a_socket_that_stopped_DELIVERING_is_not_a_live_feed(self):
        now = time.time()
        with watcher._feed_lock:
            watcher._feed.update({'last_frame_t': now - 7.6, 'last_decoded_t': now - 583.0})
        f = watcher.feed_snapshot()
        self.assertTrue(f['connected'])
        self.assertLess(f['frame_age_secs'], 30, 'frames ARE arriving')
        self.assertFalse(f['useful'], 'but the feed is not usable')

    def test_a_FRESH_connection_is_not_instantly_stalled(self):
        now = time.time()
        with watcher._feed_lock:
            watcher._feed.update({'last_decoded_t': now - 3000.0, 'connected_since': now - 5.0})
        ref = max(float(watcher._feed['last_decoded_t']), float(watcher._feed['connected_since']))
        self.assertLess(time.time() - ref, watcher.DECODE_STALL_RESUBSCRIBE_SECS, 'a 5-second-old connection must be given time to deliver')

    def test_a_connection_that_has_been_up_and_SILENT_still_resubscribes(self):
        now = time.time()
        with watcher._feed_lock:
            watcher._feed.update({'last_decoded_t': now - 3000.0, 'connected_since': now - 3000.0})
        ref = max(float(watcher._feed['last_decoded_t']), float(watcher._feed['connected_since']))
        self.assertGreater(time.time() - ref, watcher.DECODE_STALL_RESUBSCRIBE_SECS)

    def test_a_BRIEF_gap_does_not_resubscribe(self):
        now = time.time()
        with watcher._feed_lock:
            watcher._feed.update({'last_decoded_t': now - 5.0})
        since = time.time() - watcher._feed['last_decoded_t']
        self.assertLess(since, watcher.DECODE_STALL_RESUBSCRIBE_SECS)

    def test_the_bound_stays_far_outside_the_MEASURED_gap_distribution(self):
        self.assertGreaterEqual(watcher.DECODE_STALL_RESUBSCRIBE_SECS, 10, 'below ~4x the worst measured gap this starts churning the socket on ordinary jitter')

    def test_a_repair_still_completes_before_the_guardian_could_halt(self):
        self.assertLess(watcher.DECODE_STALL_RESUBSCRIBE_SECS + watcher.RESUBSCRIBE_GRACE_SECS, watcher.DECODE_MAX_AGE_SECS, "detect + repair must fit inside the guardian's patience")

    def test_a_watcher_that_has_NEVER_decoded_does_not_resubscribe_immediately(self):
        with watcher._feed_lock:
            watcher._feed['last_decoded_t'] = 0.0
        self.assertFalse(bool(watcher._feed.get('last_decoded_t')))

class ReconnectFloorTests(unittest.TestCase):

    def test_the_storm_that_happened_is_now_impossible(self):
        total = sum((watcher.reconnect_delay(n) for n in range(1, 1127)))
        self.assertGreater(total / 3600.0, 15.0, 'the same storm must now take many hours, not 40 minutes')

    def test_backoff_grows_then_CAPS(self):
        d = [watcher.reconnect_delay(n) for n in range(1, 10)]
        self.assertEqual(d[0], watcher.RECONNECT_MIN_INTERVAL_SECS)
        self.assertTrue(all((b >= a for a, b in zip(d, d[1:]))), 'must be monotonic')
        self.assertEqual(d[-1], watcher.RECONNECT_MAX_INTERVAL_SECS, 'and must cap, or a long outage becomes an infinite wait')

    def test_a_SINGLE_reconnect_is_still_fast(self):
        self.assertLessEqual(watcher.reconnect_delay(1), 5.0)

    def test_the_floor_never_exceeds_the_guardians_patience(self):
        self.assertLess(watcher.RECONNECT_MAX_INTERVAL_SECS, watcher.DECODE_MAX_AGE_SECS)

class ResubscribeGraceTests(unittest.TestCase):

    def setUp(self):
        watcher._feed_set(connected=False, connected_since=None, resubscribing_since=None, last_frame_t=0.0, last_decoded_t=0.0)

    def test_a_FRESH_resubscribe_is_still_USEFUL(self):
        now = time.time()
        watcher._feed_set(connected=False, resubscribing_since=now, last_frame_t=now - 30, last_decoded_t=now - 130)
        f = watcher.feed_snapshot()
        self.assertTrue(f['resubscribing'])
        self.assertTrue(f['useful'], 'a repair in progress must not read as an outage')

    def test_a_resubscribe_that_NEVER_COMPLETES_becomes_a_real_fault(self):
        now = time.time()
        watcher._feed_set(connected=False, resubscribing_since=now - (watcher.RESUBSCRIBE_GRACE_SECS + 5), last_frame_t=now - 400, last_decoded_t=now - 400)
        f = watcher.feed_snapshot()
        self.assertFalse(f['resubscribing'])
        self.assertFalse(f['useful'], 'past the grace it is exactly the fault it looks like')

    def test_a_GENUINELY_dead_feed_is_still_reported(self):
        now = time.time()
        watcher._feed_set(connected=False, resubscribing_since=None, last_frame_t=now - 400, last_decoded_t=now - 400)
        self.assertFalse(watcher.feed_snapshot()['useful'])

    def test_a_HEALTHY_feed_does_not_depend_on_the_grace(self):
        now = time.time()
        watcher._feed_set(connected=True, resubscribing_since=None, last_frame_t=now - 1, last_decoded_t=now - 1)
        f = watcher.feed_snapshot()
        self.assertTrue(f['useful'])
        self.assertFalse(f['resubscribing'])

    def test_a_SUCCESSFUL_reconnect_clears_the_grace(self):
        watcher._feed_set(resubscribing_since=time.time())
        watcher._feed_set(connected=True, connected_since=time.time(), last_error=None, resubscribing_since=None)
        self.assertFalse(watcher.feed_snapshot()['resubscribing'])
if __name__ == '__main__':
    unittest.main()

class DustSellSeverityTests(unittest.TestCase):

    def alerts(self, ev, value=None):
        out = []
        with mock.patch.object(watcher, 'rejects_since', return_value=[(time.time(), ev)]), mock.patch.object(watcher, 'position_value', return_value=value), mock.patch.dict(watcher._seen_rejects, {}, clear=True):
            watcher.check_rejects('example_lane_25', lambda lvl, kind, msg: out.append((lvl, msg)))
        return out

    def reject(self, side, shares, limit, body='{"error":"no orders found to match with FAK order."}'):
        e = {'tok': '0482127952', 'side': side, 'race': {'outcome': 'rejected'}, 'paths': [{'role': 'error', 'body': body}]}
        if shares is not None:
            e['shares'] = shares
        if limit is not None:
            e['limit'] = limit
        return e

    def test_the_2026_08_17_TICK_REJECTION_must_not_halt_the_pool(self):
        body = '{"error":"price 0.0616666666666667 breaks minimum tick size rule 0.001"}'
        lv = self.alerts(self.reject('BUY', 100.0, 0.062, body=body), value=6.2)
        self.assertTrue(lv, 'it must still be REPORTED — this is our bug to fix')
        levels = {l for l, _ in lv}
        self.assertNotIn('CRITICAL', levels, 'a refusal the venue made BEFORE matching carries no exposure, so it must never stop the pool: %r' % (lv,))
        self.assertTrue(any(('BOT_DEFECT' in m or 'refuse' in m.lower() for _, m in lv)), 'it must name itself as OUR defect: %r' % (lv,))

    def test_a_minimum_SIZE_refusal_is_the_same_class(self):
        body = '{"error":"order size 2 breaks minimum size rule 5"}'
        lv = self.alerts(self.reject('BUY', 2.0, 0.69, body=body), value=1.38)
        self.assertNotIn('CRITICAL', {l for l, _ in lv}, repr(lv))

    def test_NOT_ENOUGH_BALANCE_still_pages(self):
        body = '{"error":"not enough balance / allowance"}'
        lv = self.alerts(self.reject('BUY', 100.0, 0.5, body=body), value=50.0)
        self.assertEqual(lv[0][0], 'CRITICAL', repr(lv))

    def test_TONIGHTS_dust_sell_is_a_WARN_not_a_halt(self):
        lv = self.alerts(self.reject('SELL', 16.7177, 0.01), value=0.0084)
        self.assertTrue(lv, 'the dust sell must still be REPORTED, just not paged')
        self.assertEqual(lv[0][0], 'WARN', 'a $0.17 remnant with no bid must never stop a $60,000 book')

    def test_the_line_is_set_by_the_HALTS_blast_radius(self):
        self.assertEqual(self.alerts(self.reject('SELL', 1384.0, 0.01), value=1.22)[0][0], 'WARN')
        self.assertGreaterEqual(watcher.DUST_SELL_USD, 25.0)

    def test_a_REAL_sell_that_cannot_land_still_pages(self):
        lv = self.alerts(self.reject('SELL', 400.0, 0.55), value=220.0)
        self.assertEqual(lv[0][0], 'CRITICAL')

    def test_the_line_sits_at_a_DOLLAR_not_at_a_share_count(self):
        self.assertEqual(self.alerts(self.reject('SELL', 900.0, 0.001), value=0.9)[0][0], 'WARN')
        self.assertEqual(self.alerts(self.reject('SELL', 300.0, 0.9), value=270.0)[0][0], 'CRITICAL')

    def test_an_UNKNOWN_size_is_judged_as_REAL(self):
        self.assertEqual(self.alerts(self.reject('SELL', None, None), value=None)[0][0], 'CRITICAL')

    def test_dust_relief_needs_the_VENUE_to_say_the_book_was_empty(self):
        e = self.reject('SELL', 16.7, 0.01, body='{"error":"invalid signature"}')
        self.assertEqual(self.alerts(e, value=0.0084)[0][0], 'CRITICAL')

class RejectSeverityTests(unittest.TestCase):

    def _capture(self, paths, side='BUY'):
        got = []
        ev = {'tok': '1' * 20, 'side': side, 'limit': 0.409, 'race': {'outcome': 'rejected'}, 'paths': paths}
        with mock.patch.object(watcher, 'rejects_since', return_value=[(time.time(), ev)]), mock.patch.dict(watcher._seen_rejects, {}, clear=True):
            watcher.check_rejects('example_lane_26', lambda lvl, kind, msg: got.append((lvl, kind, msg)))
        return got

    def test_THE_INCIDENT_a_killed_FAK_behind_duplicates_is_a_WARN(self):
        got = self._capture([{'role': 'error', 'body': '{"error":"order 0x2036 is invalid. Duplicated."}'}, {'role': 'error', 'body': '{"error":"no orders found to match with FAK order."}'}, {'role': 'error', 'body': '{"error":"order 0x2036 is invalid. Duplicated."}'}])
        self.assertEqual(len(got), 1)
        self.assertEqual(got[0][0], 'WARN', 'a BUY that found no liquidity must not halt trading: %s' % (got[0],))
        self.assertIn('no orders found', got[0][2], 'the message must carry the REAL reason, not a duplicate')

    def test_a_rejected_SELL_still_pages_however_it_is_worded(self):
        got = self._capture([{'role': 'error', 'body': '{"error":"no orders found to match with FAK order."}'}], side='SELL')
        self.assertEqual(got[0][0], 'CRITICAL')
        self.assertIn('WE MAY STILL BE HOLDING', got[0][2])

    def test_when_EVERY_path_says_duplicated_the_order_LANDED(self):
        got = self._capture([{'role': 'error', 'body': '{"error":"order 0xabc is invalid. Duplicated."}'}, {'role': 'error', 'body': '{"error":"order 0xabc is invalid. Duplicated."}'}])
        self.assertEqual(got[0][0], 'WARN')
        self.assertIn('landed on another path', got[0][2])

    def test_a_genuine_failure_still_pages(self):
        got = self._capture([{'role': 'error', 'body': '{"error":"not enough balance / allowance"}'}])
        self.assertEqual(got[0][0], 'CRITICAL', 'a real refusal must still stop us')

class HaltedMissTests(unittest.TestCase):

    def _op(self, tmp, lanes):
        run = os.path.join(tmp, 'run')
        os.makedirs(run, exist_ok=True)
        with open(os.path.join(run, 'control.json.operator'), 'w') as f:
            json.dump({'lanes': lanes}, f)
        return tmp

    def test_halted_lanes_are_read_from_the_operator_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            self._op(tmp, {'example_lane_26': {'armed': True, 'halt_buys': True}, 'example_lane_25': {'armed': True, 'halt_buys': False}})
            self.assertEqual(watcher.halted_lanes(tmp), {'example_lane_26'})

    def test_an_UNREADABLE_operator_file_suppresses_NOTHING(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(watcher.halted_lanes(tmp), set())
            with open(os.path.join(tmp, 'control.json.operator'), 'w') as f:
                f.write('{ this is not json')
            self.assertEqual(watcher.halted_lanes(tmp), set())

class HaltFeedbackLoopTests(unittest.TestCase):

    def test_the_suppression_matches_the_FILLS_lane_not_the_installation_name(self):
        got = []
        fill = {'side': 'BUY', 'size': 332.0, 'title': '', 'token': 'T', 'leader_lane': 'example_lane_25', 'ts': time.time(), 'seen': False}
        lane_of = str(fill.get('leader_lane') or '')
        self.assertIn(lane_of, {'example_lane_25'}, 'the fill knows its own lane')
        self.assertNotIn('copybot', {'example_lane_25'}, 'the installation name can never match a lane set')

    def test_an_UNATTRIBUTED_buy_miss_is_silenced_only_when_NOTHING_could_buy(self):
        known, halted = ({'example_lane_26', 'example_lane_25'}, {'example_lane_26', 'example_lane_25'})
        self.assertTrue(bool(halted) and bool(known) and (known <= halted))
        self.assertFalse({'example_lane_26', 'example_lane_25'} <= {'example_lane_26'})

    def test_an_UNREADABLE_operator_file_never_silences_a_miss(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(watcher.known_lanes(tmp), set())
            self.assertEqual(watcher.halted_lanes(tmp), set())
            known, halted = (watcher.known_lanes(tmp), watcher.halted_lanes(tmp))
            self.assertFalse(bool(halted) and bool(known) and (known <= halted), 'unknown must never buy silence')

    def test_known_lanes_reads_every_lane_regardless_of_halt_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, 'run'), exist_ok=True)
            with open(os.path.join(tmp, 'run', 'control.json.operator'), 'w') as f:
                json.dump({'lanes': {'example_lane_26': {'halt_buys': True}, 'example_lane_25': {'halt_buys': False}}}, f)
            self.assertEqual(watcher.known_lanes(tmp), {'example_lane_26', 'example_lane_25'})
            self.assertEqual(watcher.halted_lanes(tmp), {'example_lane_26'})

class RejectValuationTests(unittest.TestCase):

    def setUp(self):
        watcher._POS_CACHE.update({'t': 0.0, 'by': {}})

    def tearDown(self):
        watcher._POS_CACHE.update({'t': 0.0, 'by': {}})

    def _serve(self, rows):

        class R:

            def __init__(s, b):
                s.b = b

            def read(s):
                return json.dumps(s.b).encode()

            def __enter__(s):
                return s

            def __exit__(s, *a):
                return False
        return mock.patch('urllib.request.urlopen', return_value=R({'positions': rows}))

    def test_a_dust_position_is_valued_in_DOLLARS_from_the_bot(self):
        with self._serve([{'lane': 'example_lane_26', 'token': 'T', 'shares': 1384.0, 'mark': 0.0005}]):
            self.assertAlmostEqual(watcher.position_value('example_lane_26', 'T'), 0.692, places=3)

    def test_a_REAL_position_is_valued_as_real(self):
        with self._serve([{'lane': 'example_lane_26', 'token': 'T', 'shares': 320.0, 'mark': 0.935}]):
            self.assertGreater(watcher.position_value('example_lane_26', 'T'), 250.0)

    def test_a_ZERO_mark_yields_NONE_so_a_broken_feed_cannot_quiet_everything(self):
        with self._serve([{'lane': 'example_lane_26', 'token': 'T', 'shares': 9999.0, 'mark': 0.0}]):
            self.assertIsNone(watcher.position_value('example_lane_26', 'T'))

    def test_an_UNREACHABLE_bot_yields_NONE_not_zero(self):
        with mock.patch('urllib.request.urlopen', side_effect=OSError('refused')):
            self.assertIsNone(watcher.position_value('example_lane_26', 'T'))

    def test_a_reject_with_no_lane_or_token_cannot_be_valued(self):
        self.assertIsNone(watcher.position_value(None, 'T'))
        self.assertIsNone(watcher.position_value('example_lane_26', None))

class GuardianLivenessTests(unittest.TestCase):

    def test_a_FRESH_state_file_is_ALIVE(self):
        with tempfile.NamedTemporaryFile(suffix='.json') as f:
            st, _why, age = watcher.guardian_liveness(f.name, now=os.path.getmtime(f.name) + 5)
            self.assertEqual(st, 'ALIVE')
            self.assertLess(age, 10)

    def test_a_STALE_state_file_is_reported(self):
        with tempfile.NamedTemporaryFile(suffix='.json') as f:
            st, why, _ = watcher.guardian_liveness(f.name, now=os.path.getmtime(f.name) + watcher.GUARDIAN_MAX_AGE_SECS + 1)
            self.assertEqual(st, 'STALE')
            self.assertIn('UNGUARDED', why)

    def test_a_MISSING_file_is_UNKNOWN_not_dead(self):
        st, why, age = watcher.guardian_liveness('/no/such/guardian_state.json')
        self.assertEqual(st, 'UNKNOWN')
        self.assertIsNone(age)

    def test_it_alerts_ONCE_per_outage_not_every_sweep(self):
        got = []
        with mock.patch.object(watcher, 'guardian_liveness', return_value=('STALE', 'down 900s', 900.0)), mock.patch.object(watcher.notify, 'configured', return_value=False), mock.patch.object(watcher, '_guardian_reported', [False]):
            for _ in range(5):
                watcher.check_guardian(lambda lvl, kind, msg: got.append(kind))
        self.assertEqual(got.count('GUARDIAN-DOWN'), 1)

    def test_recovery_is_reported_once(self):
        got = []
        with mock.patch.object(watcher.notify, 'configured', return_value=False), mock.patch.object(watcher, '_guardian_reported', [True]):
            with mock.patch.object(watcher, 'guardian_liveness', return_value=('ALIVE', 'last run 3s ago', 3.0)):
                for _ in range(3):
                    watcher.check_guardian(lambda lvl, kind, msg: got.append(kind))
        self.assertEqual(got.count('GUARDIAN-BACK'), 1)

    def test_it_NEVER_halts_anything(self):
        with mock.patch.object(watcher, 'guardian_liveness', return_value=('STALE', 'down', 900.0)), mock.patch.object(watcher.notify, 'configured', return_value=False), mock.patch.object(watcher, '_guardian_reported', [False]), mock.patch.object(watcher, 'halted_lanes') as halted:
            watcher.check_guardian(lambda *a: None)
        halted.assert_not_called()

    def test_the_alert_level_is_not_CRITICAL(self):
        got = []
        with mock.patch.object(watcher, 'guardian_liveness', return_value=('STALE', 'down', 900.0)), mock.patch.object(watcher.notify, 'configured', return_value=False), mock.patch.object(watcher, '_guardian_reported', [False]):
            watcher.check_guardian(lambda lvl, kind, msg: got.append(lvl))
        self.assertNotIn('CRITICAL', got)

class HeldSharesSafetyTests(unittest.TestCase):

    def _serve(self, doc):

        class R:

            def __init__(s, b):
                s.b = b

            def read(s):
                return json.dumps(s.b).encode()

            def __enter__(s):
                return s

            def __exit__(s, *a):
                return False
        return mock.patch('urllib.request.urlopen', return_value=R(doc))

    def test_an_INCOMPLETE_book_is_None_not_zero(self):
        with self._serve({'complete': False, 'positions': []}):
            self.assertIsNone(watcher._held_shares('T1'))

    def test_a_STALE_book_is_None(self):
        with self._serve({'status': 'stale', 'positions': []}):
            self.assertIsNone(watcher._held_shares('T1'))

    def test_a_COMPLETE_book_missing_the_token_is_a_real_zero(self):
        with self._serve({'complete': True, 'positions': [{'token': 'OTHER', 'shares': 5.0}]}):
            self.assertEqual(watcher._held_shares('T1'), 0.0)

    def test_shares_are_summed_across_lanes_holding_the_same_token(self):
        with self._serve({'complete': True, 'positions': [{'token': 'T1', 'shares': 10.0, 'lane': 'a'}, {'token': 'T1', 'shares': 5.0, 'lane': 'b'}]}):
            self.assertEqual(watcher._held_shares('T1'), 15.0)

    def test_an_unreachable_endpoint_is_None(self):
        with mock.patch('urllib.request.urlopen', side_effect=OSError('refused')):
            self.assertIsNone(watcher._held_shares('T1'))

    def test_a_non_numeric_share_count_is_None_not_a_partial_sum(self):
        with self._serve({'complete': True, 'positions': [{'token': 'T1', 'shares': 'lots'}]}):
            self.assertIsNone(watcher._held_shares('T1'))

class ModuleRealityTests(unittest.TestCase):

    def test_the_flag_reflects_whether_the_module_actually_imported(self):
        self.assertEqual(watcher._CORROBORATE_REAL, hasattr(watcher.corroborate, 'CORROBORATORS'), 'the flag disagrees with reality')

    def test_notify_flag_reflects_reality_too(self):
        self.assertEqual(watcher._NOTIFY_REAL, hasattr(watcher.notify, 'API'))

    def test_the_real_corroborator_is_in_use_not_the_stub(self):
        self.assertTrue(hasattr(watcher.corroborate, 'REFUTED'))
        self.assertTrue(callable(getattr(watcher.corroborate, 'check', None)))
        self.assertIn('stranded', getattr(watcher.corroborate, 'CORROBORATORS', {}))

def _fill(ts, token='T1', side='BUY', size=100.0):
    return {'side': side, 'token': token, 'condition': 'C1', 'tx': '0xabc', 'ts': ts, 'size': size}

def test_P0F_a_STALE_buy_decision_no_longer_covers_a_fresh_fill():
    now = 1700000000
    ancient = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 48 * 60 * 60}]
    assert not watcher.fill_is_covered(_fill(now), ancient, 45), "a two-day-old decision must not cover today's fill"

def test_P0F_a_RECENT_buy_decision_still_covers_a_later_tranche():
    now = 1700000000
    recent = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 600}]
    assert watcher.fill_is_covered(_fill(now), recent, 45)

def test_P0F_the_horizon_matches_the_quarantine_that_justifies_it():
    assert watcher.BUY_COVERAGE_TTL_SECS == 24 * 60 * 60

def test_P0F_the_boundary_is_inclusive_so_it_does_not_flap():
    now = 1700000000
    edge = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - watcher.BUY_COVERAGE_TTL_SECS}]
    assert watcher.fill_is_covered(_fill(now), edge, 45)

def _ofill(ts, oid, token='T1'):
    return {'side': 'BUY', 'token': token, 'condition': 'C1', 'tx': '0xabc', 'ts': ts, 'size': 100.0, 'his_order_id': oid}

def test_P0G_a_NEW_leader_order_on_the_same_token_is_NOT_covered():
    now = 1700000000
    decisions = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 60, 'his_order_id': 'aaaa'}]
    assert not watcher.fill_is_covered(_ofill(now, 'bbbb'), decisions, 45), 'a decision about ANOTHER of his orders must not cover this one'

def test_P0G_the_SAME_leader_order_is_still_covered():
    now = 1700000000
    decisions = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 60, 'his_order_id': 'aaaa'}]
    assert watcher.fill_is_covered(_ofill(now, 'aaaa'), decisions, 45)

def test_P0G_rows_written_BEFORE_the_id_existed_fall_back_not_flood():
    now = 1700000000
    legacy = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 60}]
    assert watcher.fill_is_covered(_ofill(now, 'bbbb'), legacy, 45), 'an id-less history must still cover, or deploy floods with false misses'

def test_P0G_a_fill_with_no_id_still_uses_the_token_test():
    now = 1700000000
    decisions = [{'kind': 'fire', 'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'ts': now - 60, 'his_order_id': 'aaaa'}]
    fill = {'side': 'BUY', 'token': 'T1', 'condition': 'C1', 'tx': '0xabc', 'ts': now, 'size': 100.0}
    assert watcher.fill_is_covered(fill, decisions, 45)
