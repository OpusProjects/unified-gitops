use std::sync::OnceLock;
use utoipa::OpenApi;

use crate::adapters::r#in::http::openapi::ApiDoc;

// The OpenAPI document, read from inside the process.
//
// Everything the browsable pages need to explain themselves is already written
// down: the `#[utoipa::path]` attribute on every handler carries the route's
// description, its parameters with their own descriptions, and what each status
// code means. That text feeds Swagger today. Reading the same document here
// means the help a page shows and the contract Swagger publishes cannot drift,
// because there is only one of them.
//
// This is how AWX does it too — its help comes from the view's docstring — and
// it is why the feature costs almost nothing: no new documentation, no second
// place to update when a parameter is added.

pub struct RouteDoc {
    /// The route pattern this path matched, e.g. /api/v1/sources/{id}/sync
    pub template: String,
    /// Methods this route answers, for the Allow line
    pub methods: Vec<&'static str>,
    /// The operation for one method
    pub description: Option<String>,
    pub parameters: Vec<ParamDoc>,
    pub responses: Vec<(String, String)>,
}

pub struct ParamDoc {
    pub name: String,
    /// "path" or "query"
    pub location: &'static str,
    pub required: bool,
    pub description: Option<String>,
}

// Parsed once. The document is built by a derive macro at compile time, but
// walking it per request to find one route would be wasteful and, worse,
// repeated work with no cache to show for it.
fn document() -> &'static utoipa::openapi::OpenApi {
    static DOC: OnceLock<utoipa::openapi::OpenApi> = OnceLock::new();
    DOC.get_or_init(ApiDoc::openapi)
}

// Templates whose last capture takes the rest of the path, slashes included
// (`{*file}` in the router; OpenAPI has no such syntax, so the spec says
// `{file}` and this list says the rest).
const GREEDY: [&str; 1] = ["/api/v1/config/{file}"];

// Does a concrete path match a route template? Segment by segment, with
// `{name}` matching exactly one segment — the same rule axum's router uses, so
// the template this finds is the template that handled the request.
fn matches(template: &str, path: &str) -> bool {
    let template = template.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    let greedy = GREEDY.contains(&template);
    let mut t = template.split('/').peekable();
    let mut p = path.split('/');
    loop {
        match (t.next(), p.next()) {
            (None, None) => return true,
            (Some(t_seg), Some(p_seg)) if greedy && t.peek().is_none() => {
                return !p_seg.is_empty() && t_seg.starts_with('{');
            }
            (Some(t), Some(p)) => {
                let wildcard = t.starts_with('{') && t.ends_with('}');
                if !wildcard && t != p {
                    return false;
                }
                if wildcard && p.is_empty() {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

// utoipa models a path item as one optional field per verb rather than a map,
// so this pairs each verb with its name in one place instead of matching on
// them at every use.
fn operations(
    item: &utoipa::openapi::path::PathItem,
) -> Vec<(&'static str, &utoipa::openapi::path::Operation)> {
    [
        ("GET", item.get.as_ref()),
        ("PUT", item.put.as_ref()),
        ("POST", item.post.as_ref()),
        ("DELETE", item.delete.as_ref()),
        ("PATCH", item.patch.as_ref()),
        ("HEAD", item.head.as_ref()),
        ("OPTIONS", item.options.as_ref()),
        ("TRACE", item.trace.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, op)| op.map(|op| (name, op)))
    .collect()
}

// What the spec says about this path, for the method the page was served with.
pub fn lookup(path: &str, method: &str) -> Option<RouteDoc> {
    let doc = document();

    // The most specific template wins. `/api/v1/config/validate` and
    // `/api/v1/config/{file}` both match the former, and the static one is the
    // route that actually ran — the same precedence axum applies.
    let (template, item) = doc
        .paths
        .paths
        .iter()
        .filter(|(template, _)| matches(template, path))
        .max_by_key(|(template, _)| -(template.matches('{').count() as i64))?;

    let ops = operations(item);
    let mut methods: Vec<&'static str> = ops.iter().map(|(name, _)| *name).collect();
    methods.sort_unstable();

    let operation = ops
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, op)| *op);

    let (description, parameters, responses) = match operation {
        Some(op) => {
            let description = op
                .description
                .clone()
                .or_else(|| op.summary.clone())
                .filter(|d| !d.trim().is_empty());

            let parameters = op
                .parameters
                .as_ref()
                .map(|params| {
                    params
                        .iter()
                        .filter_map(inline_param)
                        .map(to_param_doc)
                        .collect()
                })
                .unwrap_or_default();

            let mut responses: Vec<(String, String)> = op
                .responses
                .responses
                .iter()
                .filter_map(|(code, response)| match response {
                    utoipa::openapi::RefOr::T(response) => {
                        Some((code.clone(), response.description.clone()))
                    }
                    _ => None,
                })
                .collect();
            responses.sort_by(|a, b| a.0.cmp(&b.0));

            (description, parameters, responses)
        }
        None => (None, Vec::new(), Vec::new()),
    };

    Some(RouteDoc {
        template: template.clone(),
        methods,
        description,
        parameters,
        responses,
    })
}

// Whether a path is a GET route at all.
//
// The breadcrumb trail needs this: it used to link every ancestor of the
// current path, and some ancestors are not resources. `/api/v1/enrichers/{id}`
// has no page — only `/api/v1/enrichers/{id}/run`, which is a POST — so the
// trail offered a click that could only 405. An ancestor that is not a GET is
// still shown, just not as a link.
pub fn is_gettable(path: &str) -> bool {
    // A value in `related` can carry a query string — the oversized-response
    // page offers `…/dataset?limit=200`, which is the way out of that page and
    // therefore the last thing that should fail to be a link. The route is the
    // path; the query is arguments to it.
    let path = path.split('?').next().unwrap_or(path);
    lookup(path, "GET").is_some_and(|doc| doc.methods.contains(&"GET"))
}

// Whether a GET route takes a `limit` query parameter — i.e. whether it can be
// paged at all. Asked by the HTML layer before it offers to page something.
pub fn accepts_limit(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    lookup(path, "GET").is_some_and(|doc| {
        doc.parameters
            .iter()
            .any(|p| p.name == "limit" && p.location == "query")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_matches_a_concrete_path() {
        assert!(matches(
            "/api/v1/applications/{id}",
            "/api/v1/applications/app-a"
        ));
        assert!(matches(
            "/api/v1/applications/{id}/state/{kind}",
            "/api/v1/applications/store/state/entry"
        ));
        assert!(matches("/api/v1/applications", "/api/v1/applications"));
        // A configuration file can sit in folders.
        assert!(matches(
            "/api/v1/config/{file}",
            "/api/v1/config/applications/storage/isilon-prod.yaml"
        ));
    }

    #[test]
    fn a_template_does_not_match_a_different_shape() {
        assert!(!matches(
            "/api/v1/applications/{id}",
            "/api/v1/applications"
        ));
        assert!(!matches(
            "/api/v1/applications/{id}",
            "/api/v1/applications/app-a/sync"
        ));
        assert!(!matches("/api/v1/connectors", "/api/v1/applications"));
    }

    // A wildcard must not swallow an empty segment: //  is not an id.
    #[test]
    fn a_wildcard_needs_a_real_segment() {
        assert!(!matches(
            "/api/v1/applications/{id}",
            "/api/v1/applications/"
        ));
    }

    #[test]
    fn the_static_route_wins_over_the_capture() {
        let doc = lookup("/api/v1/config/validate", "POST").expect("a route");
        assert_eq!(doc.template, "/api/v1/config/validate");
    }

    // The point of the module: the documentation written in the handler
    // attribute is readable from here.
    #[test]
    fn a_route_carries_its_documentation_from_the_spec() {
        let doc = lookup("/api/v1/applications/app-a/sync", "POST").expect("a route");
        assert_eq!(doc.template, "/api/v1/applications/{id}/sync");
        assert!(doc.methods.contains(&"POST"));
        assert!(
            doc.parameters
                .iter()
                .any(|p| p.name == "id" && p.location == "path"),
            "the path parameter must come through"
        );
        assert!(
            doc.responses.iter().any(|(code, _)| code == "404"),
            "documented status codes must come through"
        );
    }

    #[test]
    fn allow_lists_every_method_a_route_answers() {
        let doc = lookup("/api/v1/config/config.yaml", "GET").expect("a route");
        assert!(doc.methods.contains(&"GET"));
        assert!(doc.methods.contains(&"PUT"));
        assert!(doc.methods.contains(&"DELETE"));
    }

    #[test]
    fn no_route_here_pages() {
        assert!(!accepts_limit("/api/v1/applications"));
        assert!(!accepts_limit("/api/v1/applications/app-a"));
    }

    #[test]
    fn only_gettable_paths_are_linkable() {
        assert!(is_gettable("/api/v1/applications"));
        assert!(is_gettable("/api/v1/applications/app-a"));
        assert!(is_gettable("/api/v1/applications/store/state/entry"));
        // POST-only: a link here could only ever 405
        assert!(!is_gettable("/api/v1/applications/app-a/sync"));
        // Not a route at all
        assert!(!is_gettable("/api/v1/applications/app-a/run"));
    }
}

// What can be DONE here, rather than read.
//
// Derived from the spec, not from a list someone has to remember to update: an
// action is any non-GET method on this route, plus any POST on a route one
// segment below it. That is exactly `DELETE /sources/{id}` (evict) and
// `POST /sources/{id}/sync` when you are looking at a source, and it will be
// the next such route the day someone adds one.
//
// Children that introduce a NEW path parameter are skipped — the page has no
// value to put in `{hostname}`, so `PUT /sources/{id}/hosts/{hostname}` belongs
// on a host's page, not on the source's.
pub struct Action {
    pub method: &'static str,
    /// Appended to the current concrete path to build the form's target
    pub suffix: String,
    pub description: Option<String>,
    /// Query parameters the form offers as fields, path parameters excluded
    pub parameters: Vec<ParamDoc>,
}

pub fn actions(template: &str) -> Vec<Action> {
    let doc = document();
    let mut actions = Vec::new();

    for (candidate, item) in doc.paths.paths.iter() {
        let suffix = if candidate == template {
            String::new()
        } else if let Some(rest) = candidate.strip_prefix(&format!("{template}/")) {
            // One segment below, and not a new capture
            if rest.contains('/') || (rest.starts_with('{') && rest.ends_with('}')) {
                continue;
            }
            format!("/{rest}")
        } else {
            continue;
        };

        for (method, operation) in operations(item) {
            if method == "GET" || method == "HEAD" || method == "OPTIONS" {
                continue;
            }
            // Only POST for children; the verbs on the route itself are all
            // fair game (that is where DELETE lives).
            if !suffix.is_empty() && method != "POST" {
                continue;
            }

            actions.push(Action {
                method,
                suffix: suffix.clone(),
                description: operation
                    .description
                    .clone()
                    .or_else(|| operation.summary.clone())
                    .filter(|d| !d.trim().is_empty()),
                parameters: operation
                    .parameters
                    .as_ref()
                    .map(|params| {
                        params
                            .iter()
                            .filter_map(inline_param)
                            .filter(|p| {
                                matches!(p.parameter_in, utoipa::openapi::path::ParameterIn::Query)
                            })
                            .map(to_param_doc)
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }

    actions.sort_by(|a, b| a.suffix.cmp(&b.suffix).then(a.method.cmp(b.method)));
    actions
}

/// Unwraps an inline parameter, skipping a `$ref` one.
///
/// Since utoipa 6 an operation's parameters are `RefOr<Parameter>`. The
/// derive macros only ever emit them inline, so a reference would need a
/// components lookup this page has no use for — it is dropped, the same way
/// a referenced response is.
fn inline_param(
    p: &utoipa::openapi::RefOr<utoipa::openapi::path::Parameter>,
) -> Option<&utoipa::openapi::path::Parameter> {
    match p {
        utoipa::openapi::RefOr::T(param) => Some(param),
        utoipa::openapi::RefOr::Ref(_) => None,
    }
}

fn to_param_doc(p: &utoipa::openapi::path::Parameter) -> ParamDoc {
    ParamDoc {
        name: p.name.clone(),
        location: match p.parameter_in {
            utoipa::openapi::path::ParameterIn::Query => "query",
            utoipa::openapi::path::ParameterIn::Path => "path",
            utoipa::openapi::path::ParameterIn::Header => "header",
            utoipa::openapi::path::ParameterIn::Cookie => "cookie",
            // OpenAPI 3.2's whole-query-string location; none of our routes use it.
            utoipa::openapi::path::ParameterIn::QueryString => "querystring",
        },
        required: matches!(p.required, utoipa::openapi::Required::True),
        description: p.description.clone(),
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn an_application_offers_refresh_and_sync() {
        let actions = actions("/api/v1/applications/{id}");
        for suffix in ["/refresh", "/sync"] {
            let action = actions
                .iter()
                .find(|a| a.suffix == suffix)
                .unwrap_or_else(|| panic!("{suffix} action"));
            assert_eq!(action.method, "POST");
        }
    }

    // A child that needs a path parameter we cannot fill belongs elsewhere.
    #[test]
    fn a_child_with_a_new_capture_is_not_an_action() {
        assert!(actions("/api/v1/applications").is_empty());
    }

    #[test]
    fn a_read_only_route_offers_nothing() {
        assert!(actions("/api/v1/applications/{id}/state/{kind}").is_empty());
    }
}
