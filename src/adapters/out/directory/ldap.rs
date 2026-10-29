// DirectoryPort → an LDAP server (search-then-bind, one connection per
// login). The adapter lives in auth-rs, shared with unified-api.
pub use auth_rs::ldap_directory::LdapDirectory;
