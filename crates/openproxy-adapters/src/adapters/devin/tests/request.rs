//! Request/response wire, model-uid, quota and adapter-surface tests.

#[path = "request/adapter_surface.rs"]
mod adapter_surface;
#[path = "request/build_wire.rs"]
mod build_wire;

use super::{Fv, collect, unwrap_envelope};
use bytes::Bytes;

fn decode_request(bytes: &Bytes) -> Vec<(u32, Fv)> {
    let (flag, payload) = unwrap_envelope(bytes);
    assert_eq!(flag, 0);
    collect(&payload)
}
