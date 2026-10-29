// SecretsPort — the interface every credential is resolved through — lives in
// secrets-rs, shared with unified-api. Re-exported here so the ports
// layer still lists every interface the application depends on.
pub use secrets_rs::port::{SecretsError, SecretsFuture, SecretsPort, SecretsSwapPort};
