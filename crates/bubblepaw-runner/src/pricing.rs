//! The one dated list-price table, which lives in the model catalog
//! (`bubblepaw_ai::catalog`). Delegation ledger rows, usage exports and session usage
//! all price through it; this module keeps the old paths.
pub use bubblepaw_ai::catalog::{PriceTable, Rates, TokenClasses, instant};
