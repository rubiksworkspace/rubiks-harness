//! Unified Harness Protocol server for Prime Agent: a native harness behind
//! the UHP HTTP contract at the Core conformance class. Each task runs as a
//! child `prime-agent -p --mode json` process whose event stream is translated
//! into UHP events. See the crate README for scope and non-goals.

mod catalog;
mod events;
mod protocol;
mod request;
mod runner;
mod server;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

pub use catalog::{ModelCatalog, ModelEntry};
pub use server::serve;

/// Reads the model catalog; called fresh for each request that needs it, so
/// credentials added while the server runs count without a restart. May block.
pub type CatalogSource = Arc<dyn Fn() -> ModelCatalog + Send + Sync>;

/// Everything the server needs from its composition root.
pub struct ServerConfig {
    pub host: String,
    /// Port to bind; 0 picks a free one, which the startup line reports.
    pub port: u16,
    /// Serve on a non-loopback address. The spec requires TLS there, which this
    /// server leaves to a reverse proxy.
    pub allow_remote: bool,
    pub token: String,
    /// Environment variable the token came from, removed from every task child
    /// so an agent's tools cannot read the server's credential.
    pub token_env: String,
    /// Where responses, sessions and per-session workspaces live.
    pub state_dir: PathBuf,
    /// Run every session here instead of in its own directory under `state_dir`.
    pub workspace: Option<PathBuf>,
    /// The `prime-agent` binary tasks run with.
    pub executable: PathBuf,
    /// Reported as `implementation.version` in discovery.
    pub version: String,
    pub catalog: CatalogSource,
}
