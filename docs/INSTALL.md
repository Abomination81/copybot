# Installation

This guide installs a separate instance. It does not update or connect to another operator's deployment. The supported service templates target Linux with systemd. Python 3.11+ is required for `tomllib`; the shipped runtime observers use the standard library.

## 1. Build

Install Git, Python 3.11+, a C compiler/linker, and stable Rust using your operating system's package manager and the [official Rust installer](https://www.rust-lang.org/tools/install).

```sh
git clone https://github.com/Abomination81/copybot.git
cd copybot
cargo build --locked --release --manifest-path hot/Cargo.toml --bin copybot-hot
cargo test --locked --manifest-path hot/Cargo.toml --lib --bin copybot-hot --tests
```

This builds for the machine running the command. Build on the destination architecture or use your own cross-compilation setup. No prebuilt executable is supplied in this preview.

## 2. Prepare a dedicated install

The service examples use a dedicated `copybot` user and `/opt/copybot`. Have the host administrator create that account and directory, copy this source tree there, and grant that user ownership. Do not run as root. Copy the built `hot/target/release/copybot-hot` to `/opt/copybot/bin/copybot-hot`.

From `/opt/copybot`, as the `copybot` user:

```sh
mkdir -p bin data run
cp deploy/copybot2.example.toml deploy/copybot2.toml
cp deploy/copybot.env.example deploy/copybot.env
chmod 600 deploy/copybot2.toml deploy/copybot.env
```

The source tree includes every module used by the shipped Python observers. Keep them together in `deploy/`; do not copy just `guardian.py`.

## 3. Configure your instance

Edit the local files with a private editor or approved secret manager. They are ignored by Git.

- `funder`: your custody/funding address. It is not always the signer address.
- `signer`: the address corresponding to your signing key.
- `signature_type`: the signature/custody mode actually supported by your account. Do not assume every wallet is a Safe merely because the example uses type 2.
- For a Deposit Wallet using `signature_type = 3` (`POLY_1271`), set `funder` to the Deposit Wallet contract and keep the configured `signer` as the owner EOA corresponding to `PRIVATE_KEY`. The engine uses the funder for both maker and signer inside orders, including exits, and uses the EOA for API authentication and to produce the wrapped signature. Types 0/1/2 retain the EOA as order signer. This mapping follows Polymarket's V2 builder; offline tests do not establish live CLOB acceptance. The startup `/data/orders` self-check tests API authentication only, not order acceptance.
- `PRIVATE_KEY`: your signer key, set locally. The engine derives its CLOB API credentials using the existing authentication path.
- `feed.url`: your compatible pending-transaction WebSocket feed, including its private authentication if required.
- `wallet`: the leader you intend to follow, with measured leader statistics and your own budget.
- Observer values: match `BOT_DIR`, `BOT_CONFIG`, `BOT_PORT`, `COPYBOT_API`, `OUR_WALLET`, `FILLWATCH_FUNDER`, `WATCH_WALLET`, and your RPC URL to this instance.

Read [configuration](CONFIGURATION.md). The example is deliberately incomplete: all lanes are disabled, wallet fields are symbolic, and the statistics are zero. The existing engine refuses this configuration until you supply valid values. The example is not an investment recommendation.

## 4. Establish private dashboard access

The backend binds to loopback. Use a dedicated Tailscale hostname and an access policy restricted to your operators. Tailscale Serve can terminate HTTPS and proxy to the loopback port:

```sh
sudo tailscale serve --bg http://127.0.0.1:8807
```

Confirm the resulting HTTPS hostname with `tailscale serve status`. Your dashboard is at `/pool`. Use Serve, not public Funnel. Do not open port 8807 in the cloud firewall or publish the backend through an unauthenticated proxy. Refer to the [Tailscale Serve documentation](https://tailscale.com/kb/1312/serve) for your installed version.

The application relies on this external access boundary; a private GitHub repository does not protect a deployed dashboard. The optional Cloudflare identity header in the engine is audit metadata, not standalone authentication.

## 5. Start in dry mode

Keep `bot.mode = "dry"`. Fill in valid configuration and enable only the lane you intend to observe. Have the host administrator install the included service and timer files under `/etc/systemd/system/` and run `systemctl daemon-reload`.

Start only the engine initially:

```sh
sudo systemctl start copybot-hot.service
sudo systemctl status copybot-hot.service
sudo journalctl -u copybot-hot.service -n 100 --no-pager
```

Open the private HTTPS dashboard. Check feed activity, the selected leader, mode, lane readiness, balances, and the absence of boot faults. Do not share the unredacted journal or dashboard.

## 6. Bring up the independent observers

Use the shared environment file with every service. `BOT_PORT` must match `DASHBOARD_PORT`; do not rely on their different code defaults. Start the watcher service and guardian, fillwatch, and buywatch timers from the supplied templates. Review their journals and confirm fresh observations. Enable `GUARDIAN_ENFORCE=1` when you intend its existing halt rules to operate; restart the observer processes to pick up changed environment values.

Service templates preserve the existing intervals. They are not automatically installed, enabled, or started by a build. Validate their paths and environment on your host before enabling startup at boot.

Settlement recovery runs inside the Rust engine every 60 seconds in live mode; it does not need a separate settlewatch service. If auto-redemption removes a position before the redeemable-position poll sees it, the engine uses redemption activity and a complete subsequent zero-balance snapshot to write a durable `settle` event. This closes tracked cost and books realized P&L together, with duplicate protection across polls and restarts. Pending orders, stale/incomplete snapshots, ambiguous token mappings, partial redemptions and shared token ownership defer automatic closure and emit `settle_deferred`. A missing token alone is never treated as proof of a winning payout. The history scan currently covers at most the newest 5,000 redemption rows per pass; older cases require separate investigation.

`settlewatch.py` remains a legacy repair utility for previously released positions. Its default mode is read-only. Do not run it with `--apply` while the engine is running: it appends directly to the ledger, bypassing the engine's in-memory accounting and write serialization. Installing it as an automatic writer is not part of this deployment.

The auto-redemption snapshot includes archived holdings. Direct closure requires an explicit matching `asset`, not just a default outcome index: [Polymarket's August 10 API change](https://docs.polymarket.com/changelog/predictions) documents per-outcome redemption amounts and the archived-position filter. Legacy aggregate activity and mixed histories containing both an open residual and a neutral reconciliation release require review; closing just the residual would strand the previously released cost basis.

## 7. Enable live execution deliberately

Only after checking your configuration, credentials, access controls, and observer health, change `bot.mode` to `"live"` and restart your instance. A fresh lane must then be explicitly armed through the dashboard, using the confirmation phrase shown there. No page in `docs/` can arm a bot.

Existing operator intent is persistent. An already-armed instance may resume after a restart. Use the stop controls described in [operations](OPERATIONS.md), not a restart, when you want trading stopped.
