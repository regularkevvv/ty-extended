//! WebAssembly export glue for plugins compiled to `wasm32`.
//!
//! A plugin crate compiled as a `cdylib` calls [`export_plugin!`](crate::export_plugin) to expose
//! the two functions the host's WASM runtime imports: `ty_plugin_alloc` (so the host can write a
//! request into the plugin's linear memory) and `ty_plugin_handle` (which decodes the JSON request,
//! dispatches it through [`Plugin::handle_json`](crate::Plugin::handle_json), and returns the JSON
//! response). The wire shape is JSON in and JSON out, matching `handle_json`, so the same plugin
//! type runs unchanged in-process or across the sandbox boundary.
//!
//! This module only exists on `wasm32` targets; on native targets [`export_plugin!`] expands to
//! nothing, so a plugin crate still compiles for the host (e.g. for its own unit tests).

use crate::Plugin;

/// The response returned when a request cannot be decoded or dispatched. Kept in sync with the
/// serialized form of [`PluginResponse::NoChange`](ty_plugin_protocol::PluginResponse::NoChange).
const NO_CHANGE_RESPONSE: &str = "{\"kind\":\"no-change\"}";

/// Reserve `len` bytes in the module's linear memory and return the offset the host should write to.
///
/// The buffer is deliberately leaked: the host reads it back within the same call to [`handle`],
/// and the host discards the whole store afterwards, so there is nothing to reclaim.
#[must_use]
pub fn alloc(len: u32) -> u32 {
    let mut buffer = Vec::<u8>::with_capacity(len as usize);
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr as u32
}

/// Decode the JSON request at `[ptr, ptr + len)`, dispatch it, and return a packed
/// `(response_ptr << 32) | response_len` locating the JSON response in linear memory.
///
/// The host upholds the memory contract: `ptr`/`len` describe a region previously returned by
/// [`alloc`] into which it wrote exactly `len` initialized bytes. A request that fails to decode or
/// dispatch degrades to a `no-change` response rather than trapping.
#[must_use]
#[expect(
    unsafe_code,
    reason = "reading the host-provided request buffer from linear memory requires from_raw_parts"
)]
pub fn handle<P: Plugin>(plugin: &P, ptr: u32, len: u32) -> u64 {
    // SAFETY: the host allocated this region through `ty_plugin_alloc` and wrote `len` initialized
    // bytes before calling `ty_plugin_handle`, so the slice is valid for reads of `len` bytes.
    let request = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };

    let response = match std::str::from_utf8(request) {
        Ok(request_json) => plugin
            .handle_json(request_json)
            .unwrap_or_else(|_| NO_CHANGE_RESPONSE.to_string()),
        Err(_) => NO_CHANGE_RESPONSE.to_string(),
    };

    let bytes = response.into_bytes();
    let response_len = bytes.len() as u64;
    let response_ptr = u64::from(leak(bytes));
    (response_ptr << 32) | response_len
}

/// Leak an owned byte buffer into linear memory and return its offset.
fn leak(bytes: Vec<u8>) -> u32 {
    let boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_ptr() as u32;
    std::mem::forget(boxed);
    ptr
}
