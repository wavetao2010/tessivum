# Community plugin verification

Tessivum does not host plugin code or maintain a second registry. Discovery and descriptions come from [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin); npm or immutable GitHub releases host the code. Tessivum stores only exact-version compatibility evidence in `plugins/market/compatibility.json`.

## Get listed

Submit the plugin's independent YAML entry to [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin/blob/main/contributing.md). Once merged, the Market shows the entry as **DSH community · unverified**.

## Request “Verified on Tessivum”

Open the [plugin verification request](../.github/ISSUE_TEMPLATE/plugin-verification.yml) after the community entry is live. Pin:

- community identity and source repository;
- exact npm version, Git commit, or immutable release archive;
- license and package integrity;
- target Profile and Native/WASM/Legacy Node/Browser runtime;
- `dsh.bundle`, `dsh.client`, required services/capabilities;
- minimum Tessivum version and the feature being verified.

The no-secret verification workflow checks source/package identity, Profile preflight, exact installation, Host and real-Browser startup, one declared feature, update, removal, failure rollback, console/HTTP errors, and child-process residue. Install scripts stay blocked unless separately reviewed in a restricted run.

## Statuses

- **Tessivum official**: owned and released by Tessivum.
- **Verified on Tessivum · VERSION**: that exact community release passed the recorded matrix.
- **DSH community · unverified**: listed upstream but not covered by current exact-version evidence.
- **Verification revoked · VERSION**: previous evidence was withdrawn; the ledger records why.

Verification is compatibility evidence, not a security audit or endorsement. Legacy Node and Browser plugins are trusted third-party code running with the user's permissions.

A newer release never inherits an older release's verification. The Market installs the verified exact release by default; updating to another release changes the displayed state to **unverified** until new evidence is merged. Revocation does not silently uninstall an existing plugin, but the Market displays it explicitly and subsequent installation/update remains unverified.

## Reproduce

```bash
python3 scripts/check_plugin_verification.py --network
SAMPLES=3 ./benchmarks/run-linux-container.sh
```

The first command validates the ledger against the community snapshot, npm metadata, repository, license, and downloaded tarball integrity. The second runs the no-key Linux Host/Chromium compatibility path. Raw release evidence is retained under `benchmarks/fixtures/`; the current exact-plugin result is in [the verification report](PLUGIN_VERIFICATION_REPORT.md).
