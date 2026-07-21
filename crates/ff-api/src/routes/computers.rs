//! Computer removal API routes.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
};

use crate::{error::ApiError, server::AppState};

/// `DELETE /v1/computers/{computer_id}` — remove a computer and its
/// dependent rows via [`ff_db::pg_remove_computer`]. Returns 204 on success,
/// 404 if no computer with that name exists.
pub async fn remove_computer(
    State(state): State<Arc<AppState>>,
    Path(computer_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let report = ff_db::pg_remove_computer(&state.db_pool, &computer_id).await?;
    if report.computer_rows == 0 {
        return Err(ApiError::NotFound(format!(
            "computer '{computer_id}' not found"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}
