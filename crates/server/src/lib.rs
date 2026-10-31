//! sharedmem server. Layers, lowest first: store → auth, knowledge → context, mcp → http.

pub mod auth;
pub mod context;
pub mod http;
pub mod knowledge;
pub mod mcp;
mod migrate;
pub mod store;
