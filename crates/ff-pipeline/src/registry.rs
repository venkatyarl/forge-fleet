//! Async Rust function registry used by `StepKind::RustFn`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::PipelineError;

/// Boxed async Rust function handler signature.
pub type RustFnFuture = Pin<Box<dyn Future<Output = Result<String, PipelineError>> + Send>>;

/// Handler type stored in the registry.
pub type RustFnHandler = dyn Fn(Option<String>) -> RustFnFuture + Send + Sync + 'static;

/// Name→handler registry used by the pipeline executor.
#[derive(Clone, Default)]
pub struct RustFnRegistry {
    handlers: Arc<RwLock<HashMap<String, Arc<RustFnHandler>>>>,
}

impl std::fmt::Debug for RustFnRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustFnRegistry").finish_non_exhaustive()
    }
}

impl RustFnRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a handler by name.
    pub async fn register<F, Fut>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(Option<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, PipelineError>> + Send + 'static,
    {
        let wrapped: Arc<RustFnHandler> = Arc::new(move |args| Box::pin(handler(args)));
        self.handlers.write().await.insert(name.into(), wrapped);
    }

    /// Remove a handler by name. Returns true if one existed.
    pub async fn unregister(&self, name: &str) -> bool {
        self.handlers.write().await.remove(name).is_some()
    }

    /// Clear all handlers.
    pub async fn clear(&self) {
        self.handlers.write().await.clear();
    }

    /// Resolve and invoke a handler by name.
    pub async fn call(&self, name: &str, args: Option<String>) -> Result<String, PipelineError> {
        let handler = {
            let guard = self.handlers.read().await;
            guard.get(name).cloned()
        }
        .ok_or_else(|| PipelineError::RustFnNotFound(name.to_string()))?;

        handler(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_register_call_unregister() {
        let reg = RustFnRegistry::new();

        reg.register("echo", |args| async move { Ok(args.unwrap_or_default()) })
            .await;

        let out = reg.call("echo", Some("hello".to_string())).await.unwrap();
        assert_eq!(out, "hello");

        assert!(reg.unregister("echo").await);
        let err = reg.call("echo", None).await.unwrap_err();
        assert!(matches!(err, PipelineError::RustFnNotFound(_)));
    }
}
