//! sharedmem server. Layers, lowest first: store → auth, knowledge → context, mcp → http.
//! `telemetry` holds the metrics that every layer records.

pub mod auth;
pub mod context;
pub mod http;
pub mod knowledge;
pub mod mcp;
mod migrate;
pub mod store;
pub mod telemetry;
