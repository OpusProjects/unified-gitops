use serde_json::{Map, Value};

// The `$` templates every outbound item writes: a webhook's body, an API's
// URL, a header, a polled job's condition.
//
//   $event                the whole value (a string that is ONLY this keeps
//                         its JSON type: an object stays an object)
//   $event.project        one field; `.0` indexes a list
//   ${event.project}x     braces end a name where text follows
//   $$                    a literal `$`
//
// A `$` that starts no name is text, so `costs $5` needs no escaping. Parsed
// once at load (for the checks) and again per render: both read the same
// parts, so what validates is what renders.

#[derive(Debug, Clone, PartialEq)]
pub enum Part<'a> {
    Text(&'a str),
    // `$$`, written as one `$`.
    Dollar,
    // The root and the steps below it: ["event", "project"].
    Var(Vec<&'a str>),
}

pub fn parts(template: &str) -> Vec<Part<'_>> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut text_from = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let (len, part) = match bytes.get(i + 1) {
            Some(b'$') => (2, Some(Part::Dollar)),
            Some(b'{') => match template[i + 2..].find('}') {
                Some(end) => {
                    let name = &template[i + 2..i + 2 + end];
                    match path(name) {
                        Some(steps) if steps.len() == name.split('.').count() => {
                            (end + 3, Some(Part::Var(steps)))
                        }
                        _ => (1, None),
                    }
                }
                None => (1, None),
            },
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                // A name runs over identifier characters and dots between
                // them; a trailing dot is text ("see $event.").
                let mut end = i + 1;
                while end < bytes.len() {
                    let c = bytes[end];
                    let step_char = c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
                    let dot_then_step = c == b'.'
                        && bytes
                            .get(end + 1)
                            .is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_');
                    if step_char || dot_then_step {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let name = &template[i + 1..end];
                (end - i, path(name).map(Part::Var))
            }
            _ => (1, None),
        };
        match part {
            Some(part) => {
                if text_from < i {
                    out.push(Part::Text(&template[text_from..i]));
                }
                out.push(part);
                i += len;
                text_from = i;
            }
            None => i += 1,
        }
    }
    if text_from < bytes.len() {
        out.push(Part::Text(&template[text_from..]));
    }
    out
}

fn path(name: &str) -> Option<Vec<&str>> {
    let steps: Vec<&str> = name.split('.').collect();
    let ok = steps.iter().all(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    }) && steps[0]
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
    ok.then_some(steps)
}

// Every variable a value names, in strings at any depth (keys are not
// templated: a key is the receiver's schema, not data).
pub fn vars(value: &Value) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    collect(value, &mut out);
    out
}

fn collect(value: &Value, out: &mut Vec<Vec<String>>) {
    match value {
        Value::String(s) => out.extend(string_vars(s)),
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect(v, out)),
        _ => {}
    }
}

pub fn string_vars(template: &str) -> Vec<Vec<String>> {
    parts(template)
        .into_iter()
        .filter_map(|p| match p {
            Part::Var(steps) => Some(steps.iter().map(|s| s.to_string()).collect()),
            _ => None,
        })
        .collect()
}

// What a template is checked against at load: the roots it may name here,
// and for the roots whose shape is known, the fields they have. A root not
// in `fields` takes any path (a response is whatever the receiver says).
pub struct Allowed<'a> {
    pub roots: &'a [&'a str],
    pub fields: &'a [(&'a str, &'a [&'a str])],
}

// Every variable `value` names that `allowed` does not, as messages naming
// `at` (the setting the template is in).
pub fn errors(at: &str, value: &Value, allowed: &Allowed<'_>) -> Vec<String> {
    vars(value)
        .into_iter()
        .filter_map(|steps| var_error(at, &steps, allowed))
        .collect()
}

pub fn string_errors(at: &str, template: &str, allowed: &Allowed<'_>) -> Vec<String> {
    errors(at, &Value::String(template.to_string()), allowed)
}

fn var_error(at: &str, steps: &[String], allowed: &Allowed<'_>) -> Option<String> {
    let root = steps[0].as_str();
    if !allowed.roots.contains(&root) {
        let roots: Vec<String> = allowed.roots.iter().map(|r| format!("${r}")).collect();
        return Some(format!(
            "{at} names ${}, which is not available here (use {})",
            steps.join("."),
            roots.join(", ")
        ));
    }
    let (_, fields) = allowed.fields.iter().find(|(r, _)| *r == root)?;
    let field = steps.get(1)?;
    (!fields.contains(&field.as_str())).then(|| {
        format!(
            "{at} names ${}, which ${root} has no field '{field}' for (one of: {})",
            steps.join("."),
            fields.join(", ")
        )
    })
}

// The values a render reads, by root.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    roots: Map<String, Value>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, root: &str, value: Value) -> Self {
        self.roots.insert(root.to_string(), value);
        self
    }

    pub fn set(&mut self, root: &str, value: Value) {
        self.roots.insert(root.to_string(), value);
    }

    // The value at a path; Null when any step is missing.
    pub fn lookup(&self, steps: &[&str]) -> Value {
        let mut current = match self.roots.get(steps[0]) {
            Some(v) => v,
            None => return Value::Null,
        };
        for step in &steps[1..] {
            let next = match current {
                Value::Object(map) => map.get(*step),
                Value::Array(items) => step.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => None,
            };
            match next {
                Some(v) => current = v,
                None => return Value::Null,
            }
        }
        current.clone()
    }

    // A whole value, every string in it rendered.
    pub fn render(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => self.render_value(s),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.render(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.render(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    // A string that is one variable and nothing else keeps that variable's
    // JSON type; anything else is text.
    pub fn render_value(&self, template: &str) -> Value {
        let parts = parts(template);
        if let [Part::Var(steps)] = parts.as_slice() {
            return self.lookup(steps);
        }
        Value::String(self.write(&parts))
    }

    // Always text: a URL, a header, a query parameter.
    pub fn render_text(&self, template: &str) -> String {
        self.write(&parts(template))
    }

    fn write(&self, parts: &[Part<'_>]) -> String {
        let mut out = String::new();
        for part in parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Dollar => out.push('$'),
                Part::Var(steps) => out.push_str(&as_text(&self.lookup(steps))),
            }
        }
        out
    }
}

// A value written into text: strings as they are, nothing for null, JSON
// for everything else.
pub fn as_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// A check on a value, as `wait.until` and `wait.fail_if` write it:
//
//   $poll.status == successful
//   $poll.status != running
//   $poll.status in [successful, failed]
//   $poll.status not in [pending, running]
//
// Compared as text, so `200` matches a number or a string alike.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub path: Vec<String>,
    pub op: Op,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    In,
    NotIn,
}

impl Condition {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let rest = text
            .strip_prefix('$')
            .ok_or_else(|| format!("'{text}' must start with a $variable"))?;
        let (name, rest) = rest
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("'{text}' needs an operator: ==, !=, in or not in"))?;
        let steps = path(name).ok_or_else(|| format!("'{name}' is not a variable name"))?;
        let rest = rest.trim_start();
        let (op, operand) = if let Some(r) = rest.strip_prefix("==") {
            (Op::Eq, r)
        } else if let Some(r) = rest.strip_prefix("!=") {
            (Op::Ne, r)
        } else if let Some(r) = rest.strip_prefix("not in") {
            (Op::NotIn, r)
        } else if let Some(r) = rest.strip_prefix("in") {
            (Op::In, r)
        } else {
            return Err(format!("'{text}' needs an operator: ==, !=, in or not in"));
        };
        let operand = operand.trim();
        let values: Vec<String> = match op {
            Op::In | Op::NotIn => {
                let inner = operand
                    .strip_prefix('[')
                    .and_then(|o| o.strip_suffix(']'))
                    .ok_or_else(|| format!("'{text}': in takes a list, [a, b]"))?;
                inner
                    .split(',')
                    .map(|v| unquote(v.trim()))
                    .filter(|v| !v.is_empty())
                    .collect()
            }
            Op::Eq | Op::Ne => vec![unquote(operand)],
        };
        if values.is_empty() || values.iter().any(String::is_empty) && op != Op::Eq {
            return Err(format!("'{text}' compares with nothing"));
        }
        Ok(Self {
            path: steps.iter().map(|s| s.to_string()).collect(),
            op,
            values,
        })
    }

    pub fn holds(&self, scope: &Scope) -> bool {
        let steps: Vec<&str> = self.path.iter().map(String::as_str).collect();
        let found = as_text(&scope.lookup(&steps));
        let listed = self.values.contains(&found);
        match self.op {
            Op::Eq | Op::In => listed,
            Op::Ne | Op::NotIn => !listed,
        }
    }
}

fn unquote(text: &str) -> String {
    let t = text.trim();
    for q in ['"', '\''] {
        if let Some(inner) = t.strip_prefix(q).and_then(|t| t.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> Scope {
        Scope::new()
            .with(
                "event",
                json!({"project": "infra", "files": ["a.txt", "b.txt"], "n": 3}),
            )
            .with("response", json!({"url": "/api/v2/jobs/7/"}))
    }

    #[test]
    fn a_lone_variable_keeps_its_type_and_text_is_written_into() {
        let s = scope();
        assert_eq!(
            s.render(&json!({"gitops": "$event", "files": "$event.files", "n": "$event.n"})),
            json!({
                "gitops": {"project": "infra", "files": ["a.txt", "b.txt"], "n": 3},
                "files": ["a.txt", "b.txt"],
                "n": 3,
            })
        );
        assert_eq!(
            s.render_value("$event.project: ${event.n}x, first $event.files.0."),
            json!("infra: 3x, first a.txt.")
        );
        assert_eq!(
            s.render_text("https://h$response.url"),
            "https://h/api/v2/jobs/7/"
        );
    }

    #[test]
    fn a_dollar_that_names_nothing_is_text() {
        let s = scope();
        assert_eq!(
            s.render_text("costs $5, $$event, ${nope"),
            "costs $5, $event, ${nope"
        );
        assert_eq!(s.render_text("$event.missing|$nothing"), "|");
        assert_eq!(s.render_value("$nothing"), Value::Null);
    }

    #[test]
    fn checks_name_unknown_roots_and_fields() {
        let allowed = Allowed {
            roots: &["event"],
            fields: &[("event", &["project", "files"])],
        };
        let errors = errors(
            "webhook.body",
            &json!({"a": "$event.project", "b": ["$event.prjoect"], "c": "$poll.status"}),
            &allowed,
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("$event.prjoect") || errors[1].contains("$event.prjoect"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("$poll.status") && e.contains("$event"))
        );
    }

    #[test]
    fn conditions_parse_and_hold() {
        let s = Scope::new().with("poll", json!({"status": "successful", "code": 200}));
        for (text, holds) in [
            ("$poll.status == successful", true),
            ("$poll.status != running", true),
            ("$poll.status in [failed, 'successful']", true),
            ("$poll.status not in [successful]", false),
            ("$poll.code == 200", true),
            ("$poll.missing == ''", true),
        ] {
            let c = Condition::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(c.holds(&s), holds, "{text}");
        }
        for bad in [
            "status == x",
            "$poll.status",
            "$poll.status ~ x",
            "$poll.status in x",
            "$poll.status in []",
        ] {
            assert!(Condition::parse(bad).is_err(), "{bad}");
        }
    }
}
