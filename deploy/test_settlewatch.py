from unittest.mock import patch

import settlewatch


def test_settlewatch_honors_new_and_legacy_settlements():
    rows = [
        {"ev": "settle", "lane": "test", "token": "123",
         "key": "test|redemption", "alt_key": "settle:test:123"},
        {"ev": "settle", "lane": "test", "token": "456"},
    ]
    wanted = {"settle:test:123", "settle:test:456", "settle:test:789"}
    with patch.object(settlewatch, "_rows", return_value=iter(rows)):
        assert settlewatch.already_booked(wanted) == wanted - {"settle:test:789"}


def test_no_recon_fill_is_not_an_instruction_to_credit_a_buy():
    rows = [{"ev": "fill", "lane": "test", "token": "123", "side": 0,
             "shares": 100, "price": 0.4},
            {"ev": "settle", "lane": "test", "token": "123", "payout": 1.0}]
    with patch.object(settlewatch, "_rows", return_value=iter(rows)):
        assert settlewatch.released_phantoms() == []
