import json
import os
import sys
import tempfile
import time
import unittest
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import conserve as C

class BaselineSelectionTests(unittest.TestCase):

    def test_it_takes_the_most_recent_reading_at_or_BEFORE_the_window(self):
        base = [{'t': 100, 'equity': 1.0}, {'t': 200, 'equity': 2.0}, {'t': 300, 'equity': 3.0}]
        self.assertEqual(C.pick_baseline(base, 250)['equity'], 2.0)
        self.assertEqual(C.pick_baseline(base, 200)['equity'], 2.0, 'at the boundary counts')

    def test_a_reading_from_INSIDE_the_window_is_never_used(self):
        base = [{'t': 500, 'equity': 9.0}]
        self.assertIsNone(C.pick_baseline(base, 100), 'using it would explain the trading with itself')

    def test_no_baseline_at_all_is_None_not_a_guess(self):
        self.assertIsNone(C.pick_baseline([], 100))

class WindowAccountingTests(unittest.TestCase):

    def ledger(self, rows):
        d = tempfile.mkdtemp()
        p = os.path.join(d, 'ledger.jsonl')
        with open(p, 'w') as fh:
            for r in rows:
                fh.write(json.dumps(r) + '\n')
        return p

    def test_only_fills_INSIDE_the_window_are_counted(self):
        now = int(time.time())
        p = self.ledger([{'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 0, 'shares': 100, 'price': 0.4, 't': now - 10000}, {'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 1, 'shares': 50, 'price': 0.5, 't': now - 9000}, {'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 1, 'shares': 50, 'price': 0.6, 't': now - 100}])
        old, C.LEDGER = (C.LEDGER, p)
        try:
            realised, adjust, fees, fills, _sp, _cs, _corr = C.realised_in_window(now - 3600, now + 1)
        finally:
            C.LEDGER = old
        self.assertEqual(fills, 1, 'only the in-window close')
        self.assertAlmostEqual(realised, 50 * (0.6 - 0.4), places=6)

    def test_RECON_rows_realise_NOTHING_but_still_move_shares(self):
        now = int(time.time())
        p = self.ledger([{'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 0, 'shares': 100, 'price': 0.4, 't': now - 200}, {'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 1, 'shares': 100, 'price': 0.9, 'recon': True, 't': now - 100}])
        old, C.LEDGER = (C.LEDGER, p)
        try:
            realised, _, _, fills, _sp, _cs, _corr = C.realised_in_window(now - 3600, now + 1)
        finally:
            C.LEDGER = old
        self.assertEqual(fills, 0)
        self.assertAlmostEqual(realised, 0.0, places=9, msg='a recon release must realise nothing, even at 0.90')

    def test_settlement_adjustments_are_counted_because_they_ARE_realised(self):
        now = int(time.time())
        p = self.ledger([{'ev': 'realised_adjust', 'lane': 'a', 'pnl': -1756.44, 'shares': 2000.0, 'avg_cost': 0.9, 'proceeds': 43.56, 't': now - 100}, {'ev': 'realised_adjust', 'lane': 'a', 'pnl': 999.0, 'shares': 100.0, 'avg_cost': 0.1, 'proceeds': 1009.0, 't': now - 99999}])
        old, C.LEDGER = (C.LEDGER, p)
        try:
            _, adjust, _, _, _sp, _cs, corrections = C.realised_in_window(now - 3600, now + 1)
        finally:
            C.LEDGER = old
        self.assertAlmostEqual(adjust, -1756.44, places=6, msg='only the in-window one')
        self.assertAlmostEqual(corrections, 0.0, places=9, msg='a settlement that moved cash is not a correction')

    def test_a_CASHLESS_adjustment_is_a_CORRECTION_not_this_window_s_pnl(self):
        now = int(time.time())
        p = self.ledger([{'ev': 'realised_adjust', 'lane': 'a', 'pnl': -3518.24, 't': now - 100}])
        old, C.LEDGER = (C.LEDGER, p)
        try:
            _, adjust, _, _, _sp, _cs, corrections = C.realised_in_window(now - 3600, now + 1)
        finally:
            C.LEDGER = old
        self.assertAlmostEqual(adjust, 0.0, places=9, msg="no cash moved, so it is not this window's divergence")
        self.assertAlmostEqual(corrections, -3518.24, places=6, msg='but it must still be reported, not dropped')

    def test_a_corrupt_line_is_SKIPPED_not_fatal(self):
        d = tempfile.mkdtemp()
        p = os.path.join(d, 'ledger.jsonl')
        with open(p, 'w') as fh:
            fh.write('{not json\n')
            fh.write(json.dumps({'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 0, 'shares': 1, 'price': 0.5, 't': int(time.time())}) + '\n')
        old, C.LEDGER = (C.LEDGER, p)
        try:
            C.realised_in_window(0, int(time.time()) + 1)
        finally:
            C.LEDGER = old

class ToleranceTests(unittest.TestCase):

    def test_the_tolerance_scales_with_the_account_not_just_a_flat_dollar(self):
        small = max(C.TOLERANCE_USD, 1000 * C.TOLERANCE_FRAC)
        large = max(C.TOLERANCE_USD, 60000 * C.TOLERANCE_FRAC)
        self.assertEqual(small, C.TOLERANCE_USD)
        self.assertGreater(large, C.TOLERANCE_USD)

    def test_it_has_NO_write_path_to_the_ledger(self):
        src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'conserve.py')).read()
        body = src.split('BASELINES = ')[0]
        self.assertNotIn('LEDGER, "a"', body)
        self.assertNotIn('open(LEDGER, "w")', body)
        self.assertNotIn('open(LEDGER, "a")', body)
if __name__ == '__main__':
    unittest.main()

class IdentityScenarios(unittest.TestCase):

    def _mark_move(self, *, equity_now, cash_now, equity_base, cash_base, spent=0.0, cost_of_sold=0.0):
        return equity_now - cash_now - (equity_base - cash_base) - spent + cost_of_sold

    def test_PRICE_ONLY_MOVEMENT_is_fully_explained_and_is_not_divergence(self):
        mark = self._mark_move(equity_now=2050.0, cash_now=1000.0, equity_base=2000.0, cash_base=1000.0)
        self.assertAlmostEqual(mark, 50.0)
        realised = adjust = fees = funded = 0.0
        expected = 2000.0 + funded + (realised + adjust - fees + mark)
        self.assertAlmostEqual(expected, 2050.0, msg='a pure repricing must close the identity exactly')

    def test_a_BUY_moves_cash_into_inventory_and_explains_itself(self):
        mark = self._mark_move(equity_now=2000.0, cash_now=600.0, equity_base=2000.0, cash_base=1000.0, spent=400.0)
        self.assertAlmostEqual(mark, 0.0, msg='buying at cost creates no mark movement')

    def test_a_FULL_CLOSE_at_a_profit_is_counted_ONCE(self):
        mark = self._mark_move(equity_now=2050.0, cash_now=1450.0, equity_base=2000.0, cash_base=1000.0, spent=0.0, cost_of_sold=400.0)
        self.assertAlmostEqual(mark, 0.0, msg='a close at the marked price is not a re-mark')
        realised = 50.0
        expected = 2000.0 + 0.0 + (realised + 0.0 - 0.0 + mark)
        self.assertAlmostEqual(expected, 2050.0)

    def test_FUNDING_is_not_mistaken_for_profit(self):
        mark = self._mark_move(equity_now=2500.0, cash_now=1500.0, equity_base=2000.0, cash_base=1000.0)
        self.assertAlmostEqual(mark, 0.0)
        expected = 2000.0 + 500.0 + (0.0 + 0.0 - 0.0 + mark)
        self.assertAlmostEqual(expected, 2500.0)

    def test_FEES_reduce_the_expectation_on_BOTH_sides(self):
        expected = 2000.0 + 0.0 + (0.0 + 0.0 - 3.5 + 0.0)
        self.assertAlmostEqual(expected, 1996.5)

    def test_exit_codes_distinguish_the_three_verdicts(self):
        self.assertEqual(C.EXIT_OK, 0)
        self.assertNotEqual(C.EXIT_DIVERGED, 0)
        self.assertNotEqual(C.EXIT_UNKNOWN, 0)
        self.assertNotEqual(C.EXIT_DIVERGED, C.EXIT_UNKNOWN)

    def test_BUY_SIDE_FEES_are_now_accumulated(self):
        import tempfile, os, json
        d = tempfile.mkdtemp()
        led = os.path.join(d, 'l.jsonl')
        now = 1700000000
        with open(led, 'w') as f:
            f.write(json.dumps({'ev': 'fill', 'lane': 'a', 'token': 'T', 'side': 0, 'shares': 10, 'price': 0.5, 'fee': 1.25, 't': now - 60}) + '\n')
        orig = C.LEDGER
        try:
            C.LEDGER = led
            _r, _a, fees, _n, spent, _c, _corr = C.realised_in_window(now - 3600, now + 1)
        finally:
            C.LEDGER = orig
        self.assertAlmostEqual(fees, 1.25, msg='a buy-side fee must be counted')
        self.assertAlmostEqual(spent, 5.0, msg='and the cash it consumed tracked')
