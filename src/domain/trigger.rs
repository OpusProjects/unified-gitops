use serde::Deserialize;

// Which changes an item reacts to: the files a commit touched, matched with
// globs as a repository writes them, relative to its root.
//
//   *     any characters inside one path segment
//   **    any number of whole segments, none included
//   ?     one character
//
// `paths` empty means every file; `ignore_paths` wins over `paths`, so
// `paths: [users/**]` with `ignore_paths: ["**/*.md"]` skips a README there.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathFilter {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub ignore_paths: Vec<String>,
}

impl PathFilter {
    pub fn is_everything(&self) -> bool {
        self.paths.is_empty() && self.ignore_paths.is_empty()
    }

    pub fn errors(&self, at: &str) -> Vec<String> {
        self.paths
            .iter()
            .map(|p| ("paths", p))
            .chain(self.ignore_paths.iter().map(|p| ("ignore_paths", p)))
            .filter_map(|(key, pattern)| {
                pattern_error(pattern).map(|why| format!("{at}.{key} '{pattern}' {why}"))
            })
            .collect()
    }

    // Whether this one file is selected.
    pub fn selects(&self, file: &str) -> bool {
        let wanted = self.paths.is_empty() || self.paths.iter().any(|p| glob(p, file));
        wanted && !self.ignore_paths.iter().any(|p| glob(p, file))
    }

    // The files of `changed` this selects, in their order.
    pub fn select<'a>(&self, changed: &'a [String]) -> Vec<&'a String> {
        changed.iter().filter(|f| self.selects(f)).collect()
    }
}

// What starts a run of an item that acts (an API call, a script): a change
// to the files it watches, the process starting, a cron schedule, or a
// person (the API). `paths` and `ignore_paths` are a PathFilter's.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    #[serde(default = "default_on")]
    pub on: Vec<On>,
    // Cron, five or six fields, when `on` has `schedule`.
    #[serde(default)]
    pub schedule: Option<String>,
    // Wait this long after a change, and take the changes that arrive in the
    // meantime into the same run. 0: run at once.
    #[serde(default)]
    pub debounce_seconds: u64,
}

impl Default for Trigger {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            ignore_paths: Vec::new(),
            on: default_on(),
            schedule: None,
            debounce_seconds: 0,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, serde::Serialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum On {
    // A pull brought commits touching the files it watches.
    Change,
    // The process started (its first pull done).
    Startup,
    // Its cron `schedule`.
    Schedule,
    // Someone asked: POST /api/v1/<section>/<name>/run.
    Manual,
}

impl On {
    pub fn as_str(self) -> &'static str {
        match self {
            On::Change => "change",
            On::Startup => "startup",
            On::Schedule => "schedule",
            On::Manual => "manual",
        }
    }
}

fn default_on() -> Vec<On> {
    vec![On::Change, On::Manual]
}

// The longest a debounce may hold a change back.
const MAX_DEBOUNCE_SECONDS: u64 = 3600;

impl Trigger {
    pub fn filter(&self) -> PathFilter {
        PathFilter {
            paths: self.paths.clone(),
            ignore_paths: self.ignore_paths.clone(),
        }
    }

    pub fn fires_on(&self, on: On) -> bool {
        self.on.contains(&on)
    }

    // `parse_cron` checks a schedule's syntax; it is the scheduler's.
    pub fn errors(&self, parse_cron: impl Fn(&str) -> Result<(), String>) -> Vec<String> {
        let mut errors = self.filter().errors("trigger");
        if self.on.is_empty() {
            errors.push(
                "trigger.on is empty: nothing would ever run it (change, startup, schedule, manual)"
                    .to_string(),
            );
        }
        match (&self.schedule, self.fires_on(On::Schedule)) {
            (Some(expression), true) => {
                if let Err(e) = parse_cron(expression) {
                    errors.push(format!(
                        "trigger.schedule '{expression}' is not a cron schedule: {e}"
                    ));
                }
            }
            (None, true) => {
                errors.push("trigger.on has schedule but trigger.schedule is not set".to_string())
            }
            (Some(_), false) => errors
                .push("trigger.schedule is set but trigger.on does not list schedule".to_string()),
            (None, false) => {}
        }
        if self.debounce_seconds > MAX_DEBOUNCE_SECONDS {
            errors.push(format!(
                "trigger.debounce_seconds must be at most {MAX_DEBOUNCE_SECONDS}"
            ));
        }
        errors
    }
}

fn pattern_error(pattern: &str) -> Option<&'static str> {
    if pattern.trim().is_empty() {
        Some("is empty")
    } else if pattern.starts_with('/') {
        Some("must be relative to the repository root, without a leading '/'")
    } else if pattern.split('/').any(|s| s == "..") {
        Some("must not climb out of the repository with '..'")
    } else if pattern.split('/').any(|s| s.contains("**") && s != "**") {
        Some("uses '**' inside a segment: write it alone between slashes (a/**/b)")
    } else {
        None
    }
}

// Whether `path` matches `pattern`, segment by segment.
pub fn glob(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.trim_start_matches("./").split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments(&pattern, &path)
}

fn segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((segment, path_rest)) => {
                segment_matches(first.as_bytes(), segment.as_bytes()) && segments(rest, path_rest)
            }
            None => false,
        },
    }
}

fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && segment_matches(rest, &text[1..]),
        Some((c, rest)) => text.first() == Some(c) && segment_matches(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_by_segment() {
        for (pattern, path, matches) in [
            ("users/*.json", "users/jane.json", true),
            ("users/*.json", "users/team/jane.json", false),
            ("users/**", "users/team/jane.json", true),
            ("users/**", "users", true),
            ("**/*.md", "README.md", true),
            ("**/*.md", "docs/a/b.md", true),
            ("a/**/b", "a/b", true),
            ("a/**/b", "a/x/y/b", true),
            ("v?.yaml", "v1.yaml", true),
            ("v?.yaml", "v10.yaml", false),
            ("./dns/*", "dns/zone", true),
            ("dns", "dns/zone", false),
        ] {
            assert_eq!(glob(pattern, path), matches, "{pattern} vs {path}");
        }
    }

    #[test]
    fn ignore_wins_and_empty_takes_everything() {
        let all = PathFilter::default();
        assert!(all.is_everything() && all.selects("anything"));
        let filter = PathFilter {
            paths: vec!["users/**".into()],
            ignore_paths: vec!["**/*.md".into()],
        };
        let changed: Vec<String> = ["users/a.json", "users/README.md", "roles/r.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(filter.select(&changed), [&changed[0]]);
    }

    #[test]
    fn a_trigger_runs_on_changes_and_by_hand_unless_it_says_otherwise() {
        let t = Trigger::default();
        assert!(t.fires_on(On::Change) && t.fires_on(On::Manual) && !t.fires_on(On::Startup));
        let ok = |_: &str| Ok(());
        assert!(t.errors(ok).is_empty());
        let t: Trigger = serde_saphyr::from_str(
            "on: [schedule]\nschedule: '0 * * * *'\ndebounce_seconds: 7200\n",
        )
        .unwrap();
        let e = t.errors(|_| Err("bad".to_string())).join("\n");
        assert!(e.contains("not a cron schedule: bad"), "{e}");
        assert!(e.contains("debounce_seconds"), "{e}");
        let t: Trigger = serde_saphyr::from_str("on: [schedule]\n").unwrap();
        assert!(
            t.errors(ok)
                .join("")
                .contains("trigger.schedule is not set")
        );
        let t: Trigger = serde_saphyr::from_str("on: []\nschedule: x\n").unwrap();
        let e = t.errors(ok).join("\n");
        assert!(
            e.contains("trigger.on is empty") && e.contains("does not list schedule"),
            "{e}"
        );
        assert!(serde_saphyr::from_str::<Trigger>("on: [push]\n").is_err());
    }

    #[test]
    fn patterns_that_could_never_match_are_refused() {
        let filter = PathFilter {
            paths: vec!["/abs".into(), "../up".into(), "a**".into(), " ".into()],
            ignore_paths: vec![],
        };
        assert_eq!(filter.errors("trigger").len(), 4);
    }
}
