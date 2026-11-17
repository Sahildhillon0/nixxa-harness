//! Conformance test suites, one per contract trait (feature `conformance`).
//!
//! Every implementation — real or fake — must pass its suite. Each suite is an `async fn`
//! that panics on the first violation; run it from a test with whatever executor the
//! implementation needs. Suites that need clean state take a factory and call it once per
//! scenario.
//!
//! ```ignore
//! #[tokio::test]
//! async fn sqlite_store_conforms() {
//!     nixxa_contracts::conformance::store::run(|| SqliteStore::temporary()).await;
//! }
//! ```

pub mod brain;
pub mod credentials;
pub mod knowledge;
pub mod sandbox;
pub mod store;

use crate::{AccountRef, ActionId, Timestamp, WorkspaceInfo};

pub(crate) fn workspace() -> WorkspaceInfo {
    WorkspaceInfo { name: "conformance".into(), root: std::env::temp_dir().join("nixxa-conformance") }
}

pub(crate) fn account() -> AccountRef {
    AccountRef::new("conformance-test", "default")
}

pub(crate) fn id(n: u32) -> ActionId {
    ActionId::new(format!("conformance-{n}"))
}

pub(crate) const T0: Timestamp = Timestamp(1_000);
