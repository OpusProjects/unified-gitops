pub mod api_key;
pub mod app;
pub mod connector;
pub mod credential;
pub mod desired;
pub mod ldap;
pub mod plan;
pub mod project;
pub mod run;
pub mod sync_health;
pub mod yaml;

// Default timeout for git pulls and other bounded work.
pub fn default_timeout_seconds() -> u64 {
    300
}
