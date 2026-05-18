//! `openhl-clob` — central limit orderbook matching engine.
//!
//! Pure state machine: no I/O, no async, no networking. Submit + cancel are
//! synchronous functions over plain data. Same inputs in the same order →
//! same fills out — the determinism property the chain's safety relies on.
//!
//! Integration with the rest of openhl happens at the EVM bridge: matched
//! fills become transactions that the bridge bundles into payloads. That
//! integration lives in `crates/evm/`; the matching itself is here.

pub mod book;
pub mod types;

pub use book::Book;
pub use types::{AccountId, Fill, FillResult, Order, OrderId, OrderType, Price, Qty, Side};
