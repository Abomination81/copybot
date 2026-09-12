from pathlib import Path
import check_release as check

def scan(name, text):
    return check.issues(check.ROOT / name, text)

def test_empty_secret_example_is_accepted():
    assert not scan('deploy/copybot.env.example', 'PRIVATE_KEY=\nWATCH_WALLET=\n')

def test_populated_secret_example_is_refused_without_echoing_value():
    result = scan('deploy/copybot.env.example', 'PRIVATE_KEY=synthetic-placeholder\n')
    assert result and all('synthetic-placeholder' not in reason for reason, _ in result)

def test_unknown_wallet_and_raw_abi_payload_are_refused():
    address = '1234567890abcdef' * 2 + '12345678'
    assert scan('fixture.rs', '0x' + address)
    assert scan('fixture.rs', ('0' * 24 + address) * 4)

def test_decimal_token_and_credential_patterns_are_refused():
    assert scan('fixture.rs', str(2**255 + 99))
    assert scan('fixture.txt', 'ghp_' + 'Q' * 36)

def test_public_protocol_contracts_are_preserved():
    assert not scan('protocol.rs', '0x' + sorted(check.PUBLIC)[0])

def test_live_snapshot_and_binary_artifacts_are_refused():
    assert scan('ui-mockups/live_pool.json', '{}')
    assert scan('capture.png', 'not-an-image')

def test_lockfile_hashes_are_not_treated_as_wallets():
    assert not scan('hot/Cargo.lock', 'checksum = "' + 'abcdef0123456789' * 4 + '"')
