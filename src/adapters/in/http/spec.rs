use std::sync::OnceLock;
use utoipa::OpenApi;

use crate::adapters::r#in::http::openapi::ApiDoc;

// The OpenAPI document, read from inside the process: what the browsable pages
// show about a route (its description, parameters, responses and actions)
// comes from the same document Swagger publishes, so the two cannot drift.
// The lookups are web-rs's (the `web` repository); this module builds the
// document once and answers through it.
pub use web_rs::spec::{Action, ParamDoc, RouteDoc};

fn spec() -> &'static web_rs::Spec {
    static SPEC: OnceLock<web_rs::Spec> = OnceLock::new();
    SPEC.get_or_init(|| web_rs::Spec::new(ApiDoc::openapi()))
}

// What the spec says about this path, for the method the page was served with.
pub fn lookup(path: &str, method: &str) -> Option<RouteDoc> {
    spec().lookup(path, method)
}

// Whether a path is a GET route at all, so a breadcrumb can link it.
pub fn is_gettable(path: &str) -> bool {
    spec().is_gettable(path)
}

// Whether a GET route takes `limit`, so a page can offer paging.
pub fn accepts_limit(path: &str) -> bool {
    spec().accepts_limit(path)
}

// What a page about `template` can offer to do.
pub fn actions(template: &str) -> Vec<Action> {
    spec().actions(template)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A wildcard must not swallow an empty segment: //  is not an id.

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
