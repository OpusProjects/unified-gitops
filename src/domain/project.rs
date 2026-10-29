use serde::Deserialize;

// A GitProject is a reference to a git repo that contains
// connector scripts and/or transformations.
// A single project can have multiple scripts inside
// (ex: device42/fetch.py, vmware/fetch.py, outputs/format.py)
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitProject {
    // What it is, for people: shown under its name in the UI. The name itself
    // is the item's identity (like AWX), taken by the loader.
    #[serde(default)]
    pub description: Option<String>,
    pub git_url: String,

    // Branch to clone/pull — if not specified, "main"
    #[serde(default = "default_branch")]
    pub branch: String,

    // Credentials for a private repo (GitHub token, SSH key, etc.):
    // `credentials:`, a list as on every other item, holding at most one —
    // a clone authenticates with one. Absent or empty = a public repo.
    #[serde(default)]
    pub credentials: Vec<String>,

    // DEPRECATED, still read: the singular `credential: <id>`. `deprecation`
    // says what to write instead. Read through `credential_id`.
    #[serde(default)]
    pub credential: Option<String>,

    // Seconds between periodic re-pulls (0 or None = no periodic sync).
    // Same convention as sources and enrichers.
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Cron cadence (UTC, 5-field + optional seconds) as the alternative to
    // the interval — same semantics as a source's schedule. The boot clone
    // still happens regardless: cron paces the RE-pulls.
    #[serde(default)]
    pub schedule: Option<String>,

    // Maximum seconds a clone/pull may take before it is aborted (default 300,
    // same convention as sources and enrichers). Bounds whatever awaits the
    // sync — an unreachable remote used to hang git (and its caller) forever.
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,

    // Build and maintain a Python virtualenv from the checkout's
    // requirements.txt (default false). The venv lives OUTSIDE the checkout
    // (`<projects.dir>/.venvs/<project>`), is refreshed after every pull
    // whose requirements.txt changed, and its bin/ is prepended to PATH when
    // this project's scripts run — so a `#!/usr/bin/env python3` shebang
    // resolves to the venv's interpreter and pip-installed imports work.
    // A failing install fails the project sync, visibly in its sync_health.
    #[serde(default)]
    pub python_venv: bool,

    // Update the checkout at boot? With `false` an EXISTING checkout is used
    // as-is (no network at startup) and updates happen only on demand
    // (POST /api/v1/projects/{id}/sync, e.g. from a pipeline) or on the
    // periodic interval. A MISSING checkout is always cloned regardless —
    // without the scripts there is nothing to execute.
    #[serde(default = "default_true")]
    pub sync_on_boot: bool,
}

fn default_true() -> bool {
    true
}

impl GitProject {
    // The credential it clones and pulls with, whichever form names it. Load
    // validation (`errors`) guarantees at most one does.
    pub fn credential_id(&self) -> Option<&str> {
        self.credentials
            .first()
            .or(self.credential.as_ref())
            .map(String::as_str)
    }

    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.credentials.len() > 1 {
            errors.push(format!(
                "Project '{id}' lists {} credentials — a clone authenticates with one",
                self.credentials.len()
            ));
        }
        if !self.credentials.is_empty() && self.credential.is_some() {
            errors.push(format!(
                "Project '{id}' sets both credentials and credential — keep credentials"
            ));
        }
        errors
    }

    pub fn deprecation(&self, id: &str) -> Option<String> {
        let credential = self.credential.as_ref()?;
        Some(format!(
            "project '{id}': credential is deprecated and will be refused in a later \
             release; write credentials: [{credential}]"
        ))
    }
}

// Function that returns the default value for branch.
// #[serde(default = "function_name")] calls this function
// when the field does not appear in the YAML.
fn default_branch() -> String {
    "main".to_string()
}

// "https://user:pass@host/x.git" -> "https://host/x.git". A project address
// shown back (the API, the UI) must not carry credentials someone wrote into
// it; git itself clones from the configured value, unaffected.
pub fn without_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}{}", &url[..scheme_end + 3], &rest[at + 1..]),
        None => url.to_string(),
    }
}

// Every address in a piece of text, without its userinfo: what a git error
// (its command line, and git's own stderr, which quotes the remote) becomes
// before it reaches a log, sync_health.last_error or the API.
pub fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        // The authority ends where the path, the line or the quoting does.
        let end = after
            .find(|c: char| c == '/' || c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
            .unwrap_or(after.len());
        let authority = &after[..end];
        match authority.rfind('@') {
            Some(i) => out.push_str(&authority[i + 1..]),
            None => out.push_str(authority),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_removed_from_every_address_in_a_text() {
        let text = "git clone https://bot:s3cr3t@git.example.com/x.git /p failed: \
                    fatal: unable to access 'https://bot:s3cr3t@git.example.com/x.git/': refused";
        let clean = redact_userinfo(text);
        assert!(!clean.contains("s3cr3t"), "{clean}");
        assert!(
            clean.contains("https://git.example.com/x.git /p"),
            "{clean}"
        );
        assert!(
            clean.contains("'https://git.example.com/x.git/'"),
            "{clean}"
        );
        // Nothing to remove: unchanged.
        let plain = "fetch origin main from https://git.example.com/x.git";
        assert_eq!(redact_userinfo(plain), plain);
        assert_eq!(redact_userinfo("token@x no scheme"), "token@x no scheme");
    }
}
