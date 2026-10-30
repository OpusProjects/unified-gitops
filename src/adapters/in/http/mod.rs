// The HTTP adapter (driving adapter): axum handlers, auth, routes, and
// OpenAPI spec. Each file groups handlers for a single resource.
pub mod apis;
pub mod audit;
pub mod auth;
pub mod config;
pub mod credentials;
pub mod error;
pub mod health;
pub mod html;
pub mod index;
pub mod login;
pub mod metrics;
pub mod openapi;
pub mod projects;
pub mod routes;
pub mod scripts;
pub mod session;
pub mod settings;
pub mod spec;
pub mod ui_assets;
pub mod users;
pub mod webhooks;
