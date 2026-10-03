//! WASM artifact for the Mini-Django example.
//!
//! A real plugin would implement [`ty_plugin_sdk::Plugin`] itself; this example
//! re-exports the workspace's reference implementation so the `wasm/` and
//! `monty/` examples exercise identical semantics through different runtimes.

ty_plugin_sdk::export_plugin!(ty_plugin_examples::MiniDjangoPlugin);
