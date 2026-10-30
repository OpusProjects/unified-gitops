// The application layer: the USE CASES of the service.
//
// Orchestrates domain + ports and knows nothing about HTTP or timers. Both
// HTTP handlers and the scheduler call here, so the logic of each use case
// exists in ONE place only.
pub mod actions;
pub mod config;
pub mod credentials;
pub mod http;
pub mod login;
pub mod projects;
pub mod webhooks;
