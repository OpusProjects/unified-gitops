use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use std::sync::Arc;
use utoipa::IntoParams;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::search::{FoundObject, search};

#[derive(Deserialize, IntoParams)]
pub struct SearchQuery {
    /// Text to find in an object's key or in any of its values (case-insensitive)
    pub q: String,
    /// At most this many results (default 20, at most 100)
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/v1/search",
    tag = "Applications",
    params(SearchQuery),
    responses(
        (status = 200, description = "Objects on the systems this key may see, matching by key first, then by any value: which application owns each and whether git declares it. Read from the local copies, so no system is called", body = Vec<FoundObject>),
        (status = 400, description = "Empty query", body = ErrorBody)
    )
)]
pub async fn search_objects(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<FoundObject>>, ApiError> {
    if query.q.trim().is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let config = state.config();
    let applications = config
        .applications
        .iter()
        .filter(|(id, _)| auth.permissions.allows_application(id));
    Ok(Json(search(
        &query.q,
        query.limit.unwrap_or(20).clamp(1, 100),
        applications,
        &state.described,
        state.store.as_ref(),
        &state.runs,
    )))
}
