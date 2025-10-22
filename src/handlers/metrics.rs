use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::IntoResponse;

use crate::types::AppState;

pub async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    state.prometheus_handle.run_upkeep();
    let body = state.prometheus_handle.render();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (StatusCode::OK, headers, body)
}
