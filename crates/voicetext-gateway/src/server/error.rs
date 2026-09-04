//! Small, non-secret HTTP error boundary.

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::auth::AuthenticationError;

const ERROR_CODE_HEADER: &str = "x-voicetext-error-code";

/// Safe transport error whose body never includes parser, provider, or secret details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GatewayHttpError {
    status: StatusCode,
    code: &'static str,
    retry_after_seconds: Option<u64>,
}

impl GatewayHttpError {
    pub(crate) const fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    pub(crate) const fn conflict() -> Self {
        Self::new(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT")
    }

    pub(crate) const fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "JOB_NOT_FOUND")
    }

    pub(crate) const fn unsupported_profile() -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "PROFILE_NOT_CONFIGURED")
    }

    pub(crate) const fn unavailable(code: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code).with_retry_after(1)
    }

    pub(crate) const fn rate_limited() -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "RATE_LIMIT_EXCEEDED").with_retry_after(1)
    }

    pub(crate) const fn unauthorized(_: AuthenticationError) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED")
    }

    const fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            retry_after_seconds: None,
        }
    }

    const fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error_code: &'static str,
}

impl IntoResponse for GatewayHttpError {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(ERROR_CODE_HEADER, HeaderValue::from_static(self.code));
        if let Some(seconds) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            headers.insert("retry-after", value);
        }
        (
            self.status,
            headers,
            Json(ErrorBody {
                error_code: self.code,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn response_is_bounded_and_carries_machine_code() {
        let response = GatewayHttpError::unavailable("DATABASE_UNAVAILABLE").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(ERROR_CODE_HEADER).unwrap(),
            "DATABASE_UNAVAILABLE"
        );
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
        let body = to_bytes(response.into_body(), 256).await.unwrap();
        assert_eq!(&body[..], br#"{"error_code":"DATABASE_UNAVAILABLE"}"#);
    }

    #[test]
    fn authentication_reasons_are_not_exposed() {
        for reason in [
            AuthenticationError::Missing,
            AuthenticationError::Multiple,
            AuthenticationError::Malformed,
            AuthenticationError::InvalidCredentials,
        ] {
            assert_eq!(
                GatewayHttpError::unauthorized(reason),
                GatewayHttpError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED")
            );
        }
    }
}
