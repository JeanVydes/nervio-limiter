use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderName, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;
use tracing::warn;

use crate::{
    errors::LimiterError,
    limiter::{BucketConfig, LimitEntityType, Limiter},
};

static X_RATELIMIT_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
static X_RATELIMIT_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
static X_RATELIMIT_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");

pub async fn axum_limiter_middleware(
    State((limiter, middleware_bucket_config)): State<(Arc<Mutex<Limiter>>, BucketConfig)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let key = match middleware_bucket_config.limit_by {
        LimitEntityType::Global => "_".to_string(),
        LimitEntityType::IP => addr.ip().to_string(),
        LimitEntityType::ProxiedIP => {
            request
                .headers()
                .get("X-Forwarded-For")
                .and_then(|hv| hv.to_str().ok())
                .and_then(|s| s.split(',').next()) // Get the first IP if multiple
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| addr.ip().to_string()) // Fallback to direct IP
        }
        // Handle unsupported types explicitly, though configuration should prevent this
        _ => {
            warn!(
                "Unsupported LimitEntityType configured: {:?}",
                middleware_bucket_config.limit_by
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Rate limiter misconfigured",
            )
                .into_response());
        }
    };

    let limiter_result = {
        let mut limiter_guard = limiter.lock().await;
        limiter_guard
            .limit_this(key, middleware_bucket_config)
            .await
    };

    // --- Handle Limiter Result ---
    match limiter_result {
        Ok(headers) => {
            // Store limiter headers in request extensions for potential use by handlers
            request.extensions_mut().insert(headers.clone());

            // Proceed to the next middleware/handler
            let mut response = next.run(request).await;

            if let Ok(limit_val) = HeaderValue::from_str(&headers.limit.to_string()) {
                response
                    .headers_mut()
                    .insert(X_RATELIMIT_LIMIT.clone(), limit_val);
            }
            if let Ok(remaining_val) = HeaderValue::from_str(&headers.remaining.to_string()) {
                response
                    .headers_mut()
                    .insert(X_RATELIMIT_REMAINING.clone(), remaining_val);
            }
            if let Ok(reset_val) = HeaderValue::from_str(&headers.reset.to_string()) {
                response
                    .headers_mut()
                    .insert(X_RATELIMIT_RESET.clone(), reset_val);
            }

            Ok(response)
        }
        Err(err) => {
            warn!("Rate limiting error: {:?}", err); // Log the error
                                                     // Map LimiterError to an appropriate HTTP response
            let status_code = match err {
                LimiterError::Limited
                | LimiterError::MemoryLimitExceeded
                | LimiterError::RedisMemoryExceeded
                | LimiterError::BothMemoryAndRedisMemoryExceeded => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::INTERNAL_SERVER_ERROR, // Handle other potential errors
            };
            // Return the error response directly
            Err(status_code.into_response())
        }
    }
}
