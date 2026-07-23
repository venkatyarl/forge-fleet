//! Detailed, credential-safe errors for Postgres connection attempts.

use sqlx::postgres::PgConnectOptions;

use crate::error::DbError;

pub(crate) fn parse_options(url: &str) -> Result<PgConnectOptions, DbError> {
    url.parse()
        .map_err(|error: sqlx::Error| DbError::InvalidConnectionUrl {
            reason: error.to_string(),
        })
}

pub(crate) fn connection_failed(url: &str, error: sqlx::Error) -> DbError {
    DbError::ConnectionFailed {
        endpoint: display_endpoint(url),
        reason: error.to_string(),
    }
}

pub(crate) fn failover_connection_failed(
    static_url: &str,
    static_error: &DbError,
    failover_url: &str,
    failover_error: &DbError,
) -> DbError {
    DbError::FailoverConnectionFailed {
        primary_endpoint: display_endpoint(static_url),
        primary_reason: static_error.to_string(),
        failover_endpoint: display_endpoint(failover_url),
        failover_reason: failover_error.to_string(),
    }
}

fn display_endpoint(url: &str) -> String {
    let Some((scheme, remainder)) = url.split_once("://") else {
        return "<invalid database URL>".to_owned();
    };
    let endpoint = remainder
        .rsplit_once('@')
        .map_or(remainder, |(_, endpoint)| endpoint)
        .split('?')
        .next()
        .unwrap_or_default();
    format!("{scheme}://{endpoint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_diagnostics_redact_credentials() {
        let endpoint = display_endpoint("postgres://alice:secret@db.example:5432/forgefleet");

        assert_eq!(endpoint, "postgres://db.example:5432/forgefleet");
        assert!(!endpoint.contains("alice"));
        assert!(!endpoint.contains("secret"));
    }

    #[test]
    fn invalid_url_has_a_specific_error() {
        let error = parse_options("not a postgres url").unwrap_err();

        assert!(matches!(error, DbError::InvalidConnectionUrl { .. }));
    }
}
