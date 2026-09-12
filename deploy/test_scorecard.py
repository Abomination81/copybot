import json
import os
import sys
import tempfile
import unittest
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scorecard as S

class CaseLifecycleTests(unittest.TestCase):

    def setUp(self):
        self.d = tempfile.TemporaryDirectory()
        self.p = os.path.join(self.d.name, 'cases.jsonl')

    def tearDown(self):
        self.d.cleanup()

    def meta(self, codes=('watcher_feed',), why='feed went dark'):
        return {'codes': list(codes), 'why': why, 'by': 'guardian'}

    def test_a_halt_opens_exactly_one_case(self):
        S.open_case(self.p, ['example_lane_26'], self.meta(), None, 1000.0)
        self.assertIsNone(S.open_case(self.p, ['example_lane_26', 'example_lane_25'], self.meta(), None, 1011.0), 'the same ongoing halt must not open a second case')
        self.assertEqual(len(S._read(self.p)), 1)

    def test_a_NEW_halt_after_a_clear_opens_a_new_case(self):
        S.open_case(self.p, ['example_lane_26'], self.meta(), None, 1000.0)
        S.close_case(self.p, 2000.0, buys_declined=3)
        self.assertIsNotNone(S.open_case(self.p, ['example_lane_26'], self.meta(), None, 3000.0))
        self.assertEqual(len(S._read(self.p)), 2)

    def test_the_cost_is_recorded_when_it_CLOSES_while_still_knowable(self):
        S.open_case(self.p, ['example_lane_26'], self.meta(), None, 1000.0)
        r = S.close_case(self.p, 5000.0, buys_declined=84)
        self.assertEqual(r['buys_declined'], 84)
        self.assertEqual(r['cleared_at'], 5000.0)

    def test_a_corrupt_file_does_not_lose_the_next_case(self):
        with open(self.p, 'w') as f:
            f.write('{not json\n')
        self.assertIsNotNone(S.open_case(self.p, ['example_lane_26'], self.meta(), None, 1000.0))

class JudgementTests(unittest.TestCase):

    def setUp(self):
        self.d = tempfile.TemporaryDirectory()
        self.p = os.path.join(self.d.name, 'cases.jsonl')

    def tearDown(self):
        self.d.cleanup()

    def case(self, corr=None, cost=None, cleared=True, at=1000.0):
        S.open_case(self.p, ['example_lane_26'], {'codes': ['exposure'], 'why': 'x'}, corr, at)
        if cleared:
            S.close_case(self.p, at + 60, buys_declined=cost)
        return at

    def judge(self, persists, at=1000.0):
        return S.adjudicate(self.p, at + S.ADJUDICATE_AFTER_SECS + 1, lambda _c: persists)

    def test_a_CORROBORATED_halt_is_a_TRUE_POSITIVE(self):
        self.case(corr='CONFIRMED', cost=5)
        got = self.judge(persists=False)
        self.assertEqual(got[0]['verdict'], S.TRUE_POSITIVE)

    def test_a_fault_still_present_on_re_check_is_a_TRUE_POSITIVE(self):
        self.case(cost=5)
        self.assertEqual(self.judge(persists=True)[0]['verdict'], S.TRUE_POSITIVE)

    def test_THE_FALSE_HALT_gone_uncorroborated_and_it_cost_us(self):
        self.case(cost=84)
        got = self.judge(persists=False)[0]
        self.assertEqual(got['verdict'], S.FALSE_POSITIVE)
        self.assertIn('84 leader buy', got['verdict_why'])

    def test_a_fault_that_cost_NOTHING_is_UNKNOWN_not_false(self):
        self.case(cost=0)
        self.assertEqual(self.judge(persists=False)[0]['verdict'], S.UNKNOWN)

    def test_an_UNCHECKABLE_fault_is_UNKNOWN(self):
        self.case(cost=99)
        self.assertEqual(self.judge(persists=None)[0]['verdict'], S.UNKNOWN)

    def test_a_re_check_that_RAISES_is_UNKNOWN_not_a_verdict(self):
        self.case(cost=99)

        def boom(_c):
            raise RuntimeError('api down')
        got = S.adjudicate(self.p, 1000.0 + S.ADJUDICATE_AFTER_SECS + 1, boom)
        self.assertEqual(got[0]['verdict'], S.UNKNOWN)

class NotYetJudgeableTests(unittest.TestCase):

    def setUp(self):
        self.d = tempfile.TemporaryDirectory()
        self.p = os.path.join(self.d.name, 'cases.jsonl')

    def tearDown(self):
        self.d.cleanup()

    def test_a_FRESH_case_is_left_alone(self):
        S.open_case(self.p, ['example_lane_26'], {'codes': ['x']}, None, 1000.0)
        S.close_case(self.p, 1010.0, buys_declined=1)
        self.assertEqual(S.adjudicate(self.p, 1100.0, lambda _c: False), [])

    def test_a_STILL_OPEN_halt_is_not_judged_prematurely(self):
        S.open_case(self.p, ['example_lane_26'], {'codes': ['x']}, None, 1000.0)
        self.assertEqual(S.adjudicate(self.p, 1000.0 + S.ADJUDICATE_AFTER_SECS + 1, lambda _c: False), [])

    def test_a_halt_open_for_a_DAY_is_judged_where_it_stands(self):
        S.open_case(self.p, ['example_lane_26'], {'codes': ['x']}, None, 1000.0)
        got = S.adjudicate(self.p, 1000.0 + S.STILL_OPEN_SECS + 1, lambda _c: False)
        self.assertEqual(len(got), 1)
        self.assertEqual(got[0]['verdict'], S.UNKNOWN)
        self.assertIn('outlived its cause', got[0]['verdict_why'])

    def test_a_case_is_never_judged_TWICE(self):
        S.open_case(self.p, ['example_lane_26'], {'codes': ['x']}, None, 1000.0)
        S.close_case(self.p, 1010.0, buys_declined=5)
        t = 1000.0 + S.ADJUDICATE_AFTER_SECS + 1
        self.assertEqual(len(S.adjudicate(self.p, t, lambda _c: False)), 1)
        self.assertEqual(S.adjudicate(self.p, t + 100, lambda _c: True), [], 'a verdict must not be rewritten by a later re-check')

class SummaryTests(unittest.TestCase):

    def setUp(self):
        self.d = tempfile.TemporaryDirectory()
        self.p = os.path.join(self.d.name, 'cases.jsonl')

    def tearDown(self):
        self.d.cleanup()

    def _rows(self, rows):
        with open(self.p, 'w') as f:
            for r in rows:
                f.write(json.dumps(r) + '\n')

    def test_precision_counts_only_JUDGED_cases(self):
        self._rows([{'at': 100, 'verdict': S.TRUE_POSITIVE}, {'at': 100, 'verdict': S.FALSE_POSITIVE, 'buys_declined': 84}, {'at': 100, 'verdict': S.FALSE_POSITIVE, 'buys_declined': 7}, {'at': 100, 'verdict': S.UNKNOWN}, {'at': 100, 'verdict': None}])
        s = S.summarise(self.p, 200)
        self.assertEqual((s['true'], s['false'], s['unknown']), (1, 2, 1))
        self.assertAlmostEqual(s['precision_pct'], 100.0 / 3, places=3)
        self.assertEqual(s['buys_lost_to_false_halts'], 91)

    def test_no_judged_cases_gives_no_precision_rather_than_a_fake_100(self):
        self._rows([{'at': 100, 'verdict': None}])
        self.assertIsNone(S.summarise(self.p, 200)['precision_pct'])
        self.assertIn('n/a', S.summary_line(S.summarise(self.p, 200)))

    def test_old_cases_fall_out_of_the_window(self):
        self._rows([{'at': 100, 'verdict': S.FALSE_POSITIVE, 'buys_declined': 5}])
        self.assertEqual(S.summarise(self.p, 100 + 8 * 86400)['cases'], 0)
if __name__ == '__main__':
    unittest.main()
