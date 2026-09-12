# Copybot

**Their move. Your execution.**

**Built by [Abomination81](https://github.com/Abomination81) · [X @Abomination81](https://x.com/Abomination81)**

![Copybot — low-latency copy execution, built by Abomination81](docs/assets/copybot-hero.svg)

Copybot is a self-hosted Polymarket copy-trading engine. It watches configured leaders, sizes entries against your limits, signs orders locally, and follows the configured exit rules. A Rust execution engine handles the trading path; an independent Python guardian and a browser dashboard handle oversight.

**Private preview · Linux deployment · Rust + Python · Bring your own wallet and feeds**

[Visual project guide](docs/index.html) · [Installation](docs/INSTALL.md) · [Configuration](docs/CONFIGURATION.md) · [Operations](docs/OPERATIONS.md) · [Release scope](docs/RELEASE-SCOPE.md)

## What it does

- **Fast discovery.** Multiple WebSocket feeds, duplicate-signal handling, and optional transaction-pool discovery.
- **Controlled sizing.** Per-lane budgets, price bands, copy percentages, and configurable compounding.
- **Local execution.** Local order signing, connection racing, and configured taker or maker behavior.
- **Exit management.** Sell handling, reconciliation, and restart recovery through the existing engine.
- **Independent oversight.** Guardian, watcher, fillwatch, and buywatch run separately from the trading loop.
- **One operator view.** Pool balances, lanes, positions, execution state, and incidents in the included dashboard.

This is execution software, not a source of profitable leaders. Copying a profitable account does not guarantee the same prices or results. In particular, the current runtime-lane default can flatten a position on a leader's first sell; this is not necessarily a proportional mirror of every trim. Read the execution settings before enabling a lane.

## Start here

On a Linux host with Git, a current stable Rust toolchain, and Python 3.11 or newer:

```sh
git clone https://github.com/Abomination81/copybot.git
cd copybot
cargo build --locked --release --manifest-path hot/Cargo.toml --bin copybot-hot
cp deploy/copybot2.example.toml deploy/copybot2.toml
cp deploy/copybot.env.example deploy/copybot.env
chmod 600 deploy/copybot2.toml deploy/copybot.env
```

The repository is private, so cloning requires access. Edit the two local files using the [installation guide](docs/INSTALL.md). The example intentionally has no usable wallet, leader, or feed and cannot trade as supplied. Do not paste credentials into issues, screenshots, terminal arguments, or AI chats.

There is no automatic live-trading installer. Building does not start the bot, and starting a fresh configuration does not arm a lane. Existing operator state can survive restarts—do not treat a restart as a disarm.

## Repository map

| Path | Purpose |
| --- | --- |
| `hot/src/` | Rust engine, accounting, execution, controls, and embedded regression tests |
| `hot/assets/` | Dashboard chart library, with upstream license retained |
| `deploy/dashboard/pool.html` | Actual operator dashboard; served by the bot |
| `deploy/*.py` | Independent guardian and observation processes |
| `deploy/*.service`, `deploy/*.timer` | Linux service templates |
| `deploy/*.example.*`, `deploy/*.env.example` | Credential-free configuration templates |
| `docs/` | Project page and operator instructions; no live connection |

## Verify the source

```sh
cargo test --locked --manifest-path hot/Cargo.toml --lib --bin copybot-hot
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
.venv/bin/python -m pytest deploy/ -q
python3 scripts/check_release.py
```

Only self-contained regression tests are distributed. Tests containing private chain recordings and their payloads stay out of this repository. The release preparation checks the exported Rust runtime against the local source, excluding comments and test-only code. See [release scope](docs/RELEASE-SCOPE.md) for exactly what changed in packaging and what did not.

## Privacy and access

The repository contains no operator configuration, funded-wallet identities, leader list, portfolio snapshots, trading tapes, SSH keys, or previous Git history. Public protocol addresses and provider base URLs remain where the software needs them. An independent credential scan complements the release-specific privacy checks; neither is a promise that future commits cannot leak data.

The dashboard is an operator control surface. Keep the backend on loopback and put authenticated private access in front of it. Do not expose its port directly to the internet. The documentation page is separate and has no trading controls or live data.

## Status and attribution

Private source preview. No public release, uptime guarantee, latency benchmark, or return promise is implied. A project-wide open-source license has not been selected; this is not an MIT-licensed release. Third-party terms and notices remain applicable; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Independent software. Not affiliated with or endorsed by Polymarket.
