import os
import sys
import unittest
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import preflight as P

def rec(n, **keys):
    return [dict(keys) for _ in range(n)]

class MissingFieldTests(unittest.TestCase):

    def sensor(self, requires, forbids=()):
        return P.Sensor('s', 'a', 'why', source='x', requires=requires, forbids=forbids)

    def test_THE_DUST_BUG_a_field_absent_from_every_sample_is_a_FAIL(self):
        live = rec(20, ev='clob_resp', lane='example_lane_26', limit=0.01, ok=False, paths=[], race={}, side='SELL', t=1, tok='T')
        st, why = P._check_fields(live, self.sensor(('shares',)))
        self.assertEqual(st, P.FAIL)
        self.assertIn('cannot fire', why)

    def test_a_field_present_on_every_sample_PASSES(self):
        live = rec(20, lane='example_lane_26', tok='T', shares=10.0)
        st, _ = P._check_fields(live, self.sensor(('lane', 'tok', 'shares')))
        self.assertEqual(st, P.PASS)

    def test_a_field_present_on_only_SOME_samples_still_passes(self):
        live = rec(10, lane='example_lane_26') + rec(10, lane='example_lane_26', his_order='O')
        st, _ = P._check_fields(live, self.sensor(('his_order',)))
        self.assertEqual(st, P.PASS)

class QuietPeriodTests(unittest.TestCase):

    def test_TOO_FEW_samples_is_UNKNOWN_not_FAIL(self):
        st, why = P._check_fields(rec(2, lane='m'), P.Sensor('s', 'a', 'w', requires=('shares',)))
        self.assertEqual(st, P.UNKNOWN)
        self.assertIn('proves nothing', why)

    def test_NO_samples_at_all_is_UNKNOWN(self):
        st, _ = P._check_fields([], P.Sensor('s', 'a', 'w', requires=('shares',)))
        self.assertEqual(st, P.UNKNOWN)

    def test_an_UNREADABLE_source_is_UNKNOWN_not_FAIL(self):
        st, why = P._check_fields(None, P.Sensor('s', 'a', 'w', requires=('x',)))
        self.assertEqual(st, P.UNKNOWN)
        self.assertIn('could not be read', why)

class DisjointDomainTests(unittest.TestCase):
    S = P.Sensor('d', 'a', 'w', intersects=('left', 'right'))

    def test_THE_MISS_BUG_installation_name_vs_lane_names_is_a_FAIL(self):
        st, why = P._check_domains({'left': {'copybot'}, 'right': {'example_lane_26', 'example_lane_25', 'example_lane_23'}}, self.S)
        self.assertEqual(st, P.FAIL)
        self.assertIn('share NO values', why)

    def test_overlapping_namespaces_PASS(self):
        st, _ = P._check_domains({'left': {'example_lane_26', 'example_lane_25'}, 'right': {'example_lane_26', 'example_lane_23'}}, self.S)
        self.assertEqual(st, P.PASS)

    def test_an_EMPTY_side_is_UNKNOWN_not_FAIL(self):
        st, _ = P._check_domains({'left': set(), 'right': {'example_lane_26'}}, self.S)
        self.assertEqual(st, P.UNKNOWN)

    def test_an_UNREADABLE_side_is_UNKNOWN(self):
        st, _ = P._check_domains({'left': None, 'right': {'example_lane_26'}}, self.S)
        self.assertEqual(st, P.UNKNOWN)

class RunTests(unittest.TestCase):

    def live(self):
        return {'events:fire': rec(20, ev='fire', lane='example_lane_26', tok='T', shares=1.0, his_fill=2.0, his_order='O', usd=1.0), 'events:clob_resp': rec(20, ev='clob_resp', lane='example_lane_26', tok='T', side='SELL', paths=[], limit=0.5, ok=True), 'api:/api/pool:lanes': rec(6, name='example_lane_26', leader='0xabc', pct=0.1), 'api:/api/positions:positions': rec(20, lane='example_lane_26', token='T', mark=0.5, shares=10.0)}

    def test_the_LIVE_shape_passes_every_sensor(self):
        res = P.run(lambda src: self.live().get(src), {'watcher_fill_lanes': {'example_lane_26', 'example_lane_25'}, 'operator_lane_names': {'example_lane_26', 'example_lane_25', 'example_lane_23'}})
        self.assertEqual(P.failures(res), [], P.summary(res))

    def test_dropping_ONE_key_fails_exactly_ONE_sensor(self):
        data = self.live()
        data['api:/api/pool:lanes'] = rec(6, name='example_lane_26', pct=0.1)
        res = P.run(lambda src: data.get(src), {'watcher_fill_lanes': {'example_lane_26'}, 'operator_lane_names': {'example_lane_26'}})
        bad = P.failures(res)
        self.assertEqual(len(bad), 1)
        self.assertEqual(bad[0][0].name, 'lane_leader_map')
        self.assertIn('leader', bad[0][1])

    def test_a_provider_that_RAISES_is_UNKNOWN_and_does_not_take_the_sweep_down(self):

        def boom(_src):
            raise RuntimeError('disk gone')
        res = P.run(boom, {})
        self.assertEqual(P.failures(res), [], 'a crash is not evidence an alarm is dead')
        self.assertIn('UNKNOWN', P.summary(res).upper())

    def test_summary_counts_every_sensor(self):
        res = P.run(lambda src: self.live().get(src), {'watcher_fill_lanes': {'example_lane_26'}, 'operator_lane_names': {'example_lane_26'}})
        self.assertEqual(len(res), len(P.SENSORS))

    def test_every_declared_sensor_has_a_reason_a_human_can_act_on(self):
        for s in P.SENSORS:
            self.assertTrue(s.alarm and s.why, '%s has no stated purpose' % s.name)
            self.assertTrue(s.source or s.intersects, '%s checks nothing' % s.name)
if __name__ == '__main__':
    unittest.main()
