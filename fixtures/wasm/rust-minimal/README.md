# Rust minimal WASM guest

From this directory, build the checked guest artifact exactly as follows:

```sh
rustup toolchain install 1.97.1 --component rust-std --target wasm32-unknown-unknown
cargo +1.97.1 build --locked --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/tessivum_rust_minimal.wasm plugin.wasm
shasum -a 256 plugin.wasm > plugin.wasm.sha256
```

`Cargo.lock`, `plugin.wasm`, and `plugin.wasm.sha256` are checked artifacts. Update them together only with the commands above; normal tests load the checked artifact and do not require the WASM target.

The guest uses the `tessivum-pdk` guest API only. `cordis_init` logs a fixed message through `logger@1.log`, then returns `{"abi":"cordis.plugin/v1","initialized":true}`. `cordis_call` echoes every normal payload. A payload with `{"mode":"denied"}` calls undeclared `settings@1.describe` and returns the host denial as `{"denial":{"code":"SERVICE_PERMISSION_DENIED"}}`; `{"mode":"trap"}` intentionally traps. The event, update, and stop exports respectively return `{"accepted":true}`, `{"updated":true}`, and `{"stopped":true}`.
