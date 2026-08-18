# Rust minimal WASM guest

From this directory, build the checked guest artifact exactly as follows:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/tessivum_rust_minimal.wasm plugin.wasm
shasum -a 256 plugin.wasm > plugin.wasm.sha256
```

`plugin.wasm` and `plugin.wasm.sha256` are checked artifacts. Update them together only with the commands above; this source-only change intentionally does not create either file so the integration orchestrator can build them after the final wiring lands.

The guest uses the `tessivum-pdk` guest API only. `cordis_init` logs a fixed message through `logger@1.log`, then returns `{"abi":"cordis.plugin/v1","initialized":true}`. `cordis_call` echoes every normal payload. A payload with `{"mode":"denied"}` calls undeclared `settings@1.describe` and returns the host denial as `{"denial":{"code":"SERVICE_PERMISSION_DENIED"}}`; `{"mode":"trap"}` intentionally traps. The event, update, and stop exports respectively return `{"accepted":true}`, `{"updated":true}`, and `{"stopped":true}`.
