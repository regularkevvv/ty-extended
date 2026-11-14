//! Example `ty` plugins built entirely on top of [`ty_plugin_sdk`].
//!
//! These mirror the three semantic hooks the fork implements against the mock runtime and exist
//! to demonstrate the authoring surface end to end. Each module exposes a plugin type that
//! implements [`ty_plugin_sdk::Plugin`]; the crate's tests drive them through
//! [`Plugin::handle`](ty_plugin_sdk::Plugin::handle) and assert the responses.
//!
//! The important structural property is in `Cargo.toml`: this crate depends only on the SDK
//! (which depends only on the protocol crate). No example reaches into `ty`'s checker internals.

pub mod call_return;
pub mod class_transform;
pub mod minidjango;
pub mod stub_overlay;

pub use call_return::FieldCallReturnPlugin;
pub use class_transform::ModelClassTransformPlugin;
pub use minidjango::MiniDjangoPlugin;
pub use stub_overlay::StubOverlayPlugin;
