# What this package changes—and what it does not

This is a clean source distribution, not a trading rewrite.

## Preserved

- Every production Rust module used by `copybot-hot`.
- Order signing, protocol constants, feed handling, sizing, execution, ledger, reconciliation, guardian rules, and dashboard calculations.
- Rust dependency versions from the local lockfile.
- The actual operator dashboard and the chart library's upstream notice.
- Self-contained regression tests that do not carry private chain data.

The private preparation check compares canonical Rust syntax after removing comments and test-only modules from both trees. All production modules must match. Python executable structure is compared separately; only identity/configuration strings and documentation are scrubbed. Deployment paths and users are templated for a new install.

## Excluded

- Previous Git history and private author/host metadata.
- Populated environment/configuration, wallets, leader identities, and operator intent.
- Trading tapes, ABI transaction payloads, token lists, portfolio snapshots, P&L exports, and simulator state.
- Private research, incident narratives, historical measurements, screenshots, and audit handoffs.
- Diagnostic/manual-trading binaries outside the production package's single-executable scope.
- Tests whose recorded-chain fixtures would identify accounts or markets.

Research-heavy source comments and docstrings are removed in the export; maintainers keep the original annotated source privately. The GitHub package has new standalone documentation. The tests shipped here are not the complete private regression corpus. No deleted test is counted as passing.

## Configuration changes only

Operator-specific Python fallback wallets are emptied, private identity labels are anonymized, and host paths are replaced with `/opt/copybot`. The new operator must supply their values through the existing configuration mechanisms. No credentials are generated or retrieved for them. Default numerical trading rules in production source are not rewritten.

The example configuration is illustrative and deliberately unusable until completed. It is not the previous operator's strategy. The package does not install or start services, arm a lane, transfer funds, rotate credentials, change an existing repository, or publish the documentation website.

## Private preview

The repository is private. `docs/index.html` is a static presentation/installation page prepared for a future publication decision; it has no API connection, tracking, wallet integration, or live data. GitHub Pages is not enabled by this package. Private repository visibility does not by itself imply that a Pages website would be private.
