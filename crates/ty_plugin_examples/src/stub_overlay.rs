//! Example: a stub-overlay plugin.
//!
//! Stub overlays are resolved at the module-resolver layer, so this plugin contributes no request
//! hooks — its entire job is to declare, in its manifest, that it supplies an extra `.pyi` overlay
//! for a claimed third-party module. It is the least invasive kind of plugin.

use ty_plugin_sdk::protocol::PluginManifest;
use ty_plugin_sdk::{ManifestBuilder, Plugin};

/// The module this example overlays.
pub const OVERLAY_MODULE: &str = "toy";
/// The overlay artifact path, resolved relative to the plugin/config root by the host.
pub const OVERLAY_PATH: &str = "stubs/toy.pyi";

/// A plugin that augments the `toy` module with an additional stub overlay.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubOverlayPlugin;

impl Plugin for StubOverlayPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("example.stub-overlay", "Toy stub overlay", "0.1.0")
            .stub_overlay(OVERLAY_MODULE, OVERLAY_PATH)
            .build()
    }
}
