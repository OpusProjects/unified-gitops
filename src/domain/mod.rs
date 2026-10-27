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
pub mod webhook;
pub mod yaml;

// The wall clock now, in unix seconds: when a run started, as a history
// reports it. A clock before 1970 reads as 0 rather than failing a pull.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Default timeout for git pulls and other bounded work.
pub fn default_timeout_seconds() -> u64 {
    300
}
