//! Re-exports del dispatcher modular (`crate::dispatcher::*`).
//!
//! Mantiene la API pública intacta para call sites externos
//! (`stages/target.rs:339`, `pipeline.rs:115,169`, `streaming_state.rs:288,322,323,346`).

pub use crate::dispatcher::{
    Dispatcher, UpstreamDispatcher,
};
// Fachada de re-exports: preserva el contrato `crate::upstream_dispatcher::*`
// aunque algunos tipos aún no tengan call sites activos en este crate.
#[allow(unused_imports)]
pub(crate) use crate::dispatcher::{
    DispatchContext, DispatchParams, NonStreamingSuccessArgs,
    ProxyRotationTrigger, StreamDispatchParams, StreamFailureContext,
    StreamingNon2xxArgs, StreamingSuccessArgs,
};