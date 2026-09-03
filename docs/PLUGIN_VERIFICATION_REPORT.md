# Plugin verification report: DSH-better-sidebar 0.16.1

Date: 2026-09-03  
Tessivum: `0.1.0-alpha.23`  
Target Profile: `web`  
Runtime: Legacy Node Host + Browser Cordis

## Immutable identity

- Community entry: `DSH-better-sidebar`, owner `omdsh-dev`
- Repository: `https://github.com/omdsh-dev/DSH-better-sidebar`
- npm: `dsh-better-sidebar@0.16.1`
- License: MIT
- Integrity: `sha512-fjFNzfrgdIbzlcC4Sd4aS1I2ZRbuA+/m3XQnOxY13jE6IKJzwz0+GjATcKTyFoLnXoDRp2QJz/U0GxhaOD70Dw==`
- Required Browser injects: runtime, locale, slots, conversation, modules

`python3 scripts/check_plugin_verification.py --network` confirms that the community identity, npm repository, version, license, registry integrity and downloaded tarball digest match the ledger.

Published evidence:

- [Lifecycle summary JSON](../plugins/market/evidence/dsh-better-sidebar-0.16.1.json) — SHA-256 `4802e4732277a89c96e4b7121ebfc88d89fceaa957cc2b52f531e5acc52475f5`
- [Product/Chromium raw JSON](../plugins/market/evidence/dsh-better-sidebar-0.16.1-product.json) — SHA-256 `b477d3246b1d180ffa823f6c62b66bcfd4eb56367012edecea8498f89a04797c`

## Matrix result

| Check | Result |
|---|---|
| Community entry is present and not shadowed by the official catalog | pass |
| Profile preflight and exact `0.16.1` installation | pass |
| Market install request resolves to `dsh-better-sidebar@0.16.1` | pass |
| Legacy Host starts after restart | pass |
| Real Chromium receives the `dsh-better-sidebar` boot entry | pass |
| Browser mounts one visible plugin-owned `[data-dsh-panel-host]` surface | pass |
| Browser boot has no exception, console error or failed plugin asset | pass |
| Exact update to `0.17.1` records `0.17.1` | pass; state becomes unverified |
| Removal clears dependency, bundle row and package link | pass |
| Failed `99.99.99` install restores both profile files byte-for-byte | pass |
| Headless, Chromium and Web Host exit gracefully without forced cleanup or residue | pass |

Observed Browser boot entry:

```json
{
  "id": "dsh-better-sidebar",
  "url": "/plugins/dsh-better-sidebar/client.js?rev=3ae56a444a61",
  "inject": [
    "@deepseek-ai/dsh-client-runtime",
    "@deepseek-ai/dsh-client-locale",
    "@deepseek-ai/dsh-client-ui-slots",
    "@deepseek-ai/dsh-client-ui-conversation",
    "@deepseek-ai/dsh-client-modules"
  ]
}
```

The fixed-Linux publication matrix recorded 30/30 successful Compatibility Host/Chromium samples, completed ten resident Sessions in every sample, and left zero post-dispose process residue. The raw product evidence and statistics are published in [`PHASE9_BENCHMARK_REPORT.md`](PHASE9_BENCHMARK_REPORT.md).

## Trust boundary

This result establishes compatibility only for `dsh-better-sidebar@0.16.1` on Tessivum `0.1.0-alpha.23`. It is not a security audit. `0.17.1`, prereleases, source checkouts and other versions remain unverified. The plugin executes as trusted third-party Legacy Node/Browser code with the user's permissions.
