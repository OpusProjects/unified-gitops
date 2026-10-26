// The HTTP adapter (driving adapter): axum handlers, auth, routes, and
// OpenAPI spec. Each file groups handlers for a single resource.
pub mod applications;
pub mod audit;
pub mod auth;
pub mod config;
pub mod connectors;
pub mod credentials;
pub mod error;
pub mod health;
pub mod html;
pub mod index;
pub mod login;
pub mod metrics;
pub mod notifications;
pub mod openapi;
pub mod projects;
pub mod routes;
pub mod search;
pub mod session;
pub mod settings;
pub mod spec;
pub mod ui_assets;
pub mod users;
