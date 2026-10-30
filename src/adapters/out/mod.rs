// Outbound (driven) adapters: the app drives the outside world through these.
// Each implements a driven port from `ports/`.
pub mod config;
pub mod directory;
pub mod git;
pub mod python;
pub mod secrets;
pub mod webhooks;
