//! Memory API routes.
//!
//! Read-only views over the ff-memory realm/node graph plus the global
//! user model built by [`ff_memory::MemoryStore::build_global_user_model`].
//! All routes return 503 when the API runs without a configured memory store.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use ff_memory::{MemoryStore, NodeId, Realm, RealmId, UserModel, UserModelNode};

use crate::{error::ApiError, server::AppState};

/// Query parameters shared by the memory list endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryListQuery {
    /// Restrict results to a single realm.
    pub realm_id: Option<Uuid>,
}

#[derive(FromRow)]
struct RealmRow {
    id: Uuid,
    name: String,
}

#[derive(FromRow)]
struct NodeRow {
    id: Uuid,
    realm_id: Uuid,
    content: Value,
}

fn memory_store(state: &AppState) -> Result<&Arc<MemoryStore>, ApiError> {
    state
        .memory_store
        .as_ref()
        .ok_or_else(|| ApiError::BackendUnavailable("memory store not configured".to_string()))
}

/// List memory realms, optionally filtered by `realm_id`.
pub async fn list_realms(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MemoryListQuery>,
) -> Result<Json<Vec<Realm>>, ApiError> {
    let store = memory_store(&state)?;
    let rows = match query.realm_id {
        Some(realm_id) => {
            sqlx::query_as::<_, RealmRow>("SELECT id, name FROM memory_realms WHERE id = $1")
                .bind(realm_id)
                .fetch_all(store.pool())
                .await
        }
        None => {
            sqlx::query_as::<_, RealmRow>("SELECT id, name FROM memory_realms")
                .fetch_all(store.pool())
                .await
        }
    }
    .map_err(|error| ApiError::internal(error.to_string()))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| Realm {
                id: RealmId(row.id),
                name: row.name,
            })
            .collect(),
    ))
}

/// List memory nodes, optionally filtered by `realm_id`.
pub async fn list_nodes(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MemoryListQuery>,
) -> Result<Json<Vec<UserModelNode>>, ApiError> {
    let store = memory_store(&state)?;
    let rows = match query.realm_id {
        Some(realm_id) => {
            sqlx::query_as::<_, NodeRow>(
                "SELECT id, realm_id, content FROM memory_nodes WHERE realm_id = $1",
            )
            .bind(realm_id)
            .fetch_all(store.pool())
            .await
        }
        None => {
            sqlx::query_as::<_, NodeRow>("SELECT id, realm_id, content FROM memory_nodes")
                .fetch_all(store.pool())
                .await
        }
    }
    .map_err(|error| ApiError::internal(error.to_string()))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| UserModelNode {
                id: NodeId(row.id),
                realm_id: RealmId(row.realm_id),
                content: row.content,
            })
            .collect(),
    ))
}

/// Build and return the deduplicated global user model.
pub async fn user_model(State(state): State<Arc<AppState>>) -> Result<Json<UserModel>, ApiError> {
    let store = memory_store(&state)?;
    let model = store
        .build_global_user_model()
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(model))
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use ff_security::auth::{Scope, generate_api_key};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        registry::BackendRegistry,
        server::{AppState, build_http_router},
    };

    fn test_app() -> (Router, String) {
        let registry = Arc::new(BackendRegistry::new(Vec::new()));
        let (token, key) = generate_api_key("user", vec![Scope::Read], None);
        let state = Arc::new(AppState::new(registry, vec![key]).unwrap());
        (build_http_router(state, &[]), token)
    }

    fn authenticated_request(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn memory_routes_require_authentication() {
        for uri in ["/memory/realms", "/memory/nodes", "/memory/user_model"] {
            let (app, _) = test_app();
            let response = app
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test]
    async fn memory_routes_return_503_without_configured_store() {
        for uri in [
            "/memory/realms",
            "/memory/nodes",
            "/memory/nodes?realm_id=00000000-0000-0000-0000-000000000001",
            "/memory/user_model",
        ] {
            let (app, token) = test_app();
            let response = app
                .oneshot(authenticated_request(uri, &token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }

    #[tokio::test]
    async fn list_endpoints_reject_malformed_realm_id() {
        for uri in [
            "/memory/realms?realm_id=not-a-uuid",
            "/memory/nodes?realm_id=not-a-uuid",
        ] {
            let (app, token) = test_app();
            let response = app
                .oneshot(authenticated_request(uri, &token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }
}
