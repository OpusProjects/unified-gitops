// Outbound (driven) adapters: the app drives the outside world through these.
// Each implements a driven port from `ports/`.
pub mod config;
pub mod directory;
pub mod git;
pub mod http_client;
pub mod process;
// Not a port implementation: the scrubbed-environment Command builder.
pub(crate) mod process_env;
pub mod python;
pub mod secrets;
