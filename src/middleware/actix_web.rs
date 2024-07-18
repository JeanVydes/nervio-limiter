use std::future::{ready, Ready};

use actix_web::{
    body::{BoxBody, EitherBody},
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    http::{self, header::HeaderValue},
    Error, HttpMessage, HttpResponse,
};
use futures_util::future::LocalBoxFuture;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{errors::LimiterError, limiter::{LimitEntityType, LimitThisConfig, Limiter, LimiterHeaders}};

#[derive(Debug, Clone)]
pub struct ActixWebLimiterMiddleware {
    pub limiter: Arc<Mutex<Limiter>>,
    pub middleware_limit_config: LimitThisConfig,
}

impl<S, B> Transform<S, ServiceRequest> for ActixWebLimiterMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type InitError = ();
    type Transform = ActixWebLimiterService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        if self.middleware_limit_config.limit_by != LimitEntityType::Global
            && self.middleware_limit_config.limit_by != LimitEntityType::IP
        {
            panic!("Invalid limit_by value. Only Global and IP are supported.");
        }

        ready(Ok(ActixWebLimiterService {
            service: Arc::new(service),
            limiter: self.limiter.clone(),
            middleware_limit_config: self.middleware_limit_config.clone(),
        }))
    }
}

pub struct ActixWebLimiterService<S> {
    pub service: Arc<S>,
    pub limiter: Arc<Mutex<Limiter>>,
    pub middleware_limit_config: LimitThisConfig,
}

impl<S, B> Service<ServiceRequest> for ActixWebLimiterService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let limiter = self.limiter.clone();
        let service = self.service.clone();
        let middleware_limit_config = self.middleware_limit_config.clone();

        Box::pin(async move {
            let mut limiter = limiter.lock().await;
            let (pass, limiter_result) = match middleware_limit_config.limit_by {
                LimitEntityType::Global => {
                    let result = limiter
                        .limit_this(
                            "_".to_owned(),
                            LimitThisConfig {
                                name: middleware_limit_config.name,
                                limit_by: LimitEntityType::Global,
                                max_requests_per_cycle: middleware_limit_config
                                    .max_requests_per_cycle,
                                cycle_duration: middleware_limit_config.cycle_duration,
                            },
                        )
                        .await;
                    (false, result)
                }
                LimitEntityType::IP => {
                    let ip = req
                        .connection_info()
                        .realip_remote_addr()
                        .unwrap_or("_")
                        .to_owned();

                    let result = limiter
                        .limit_this(
                            ip,
                            LimitThisConfig {
                                name: middleware_limit_config.name,
                                limit_by: LimitEntityType::IP,
                                max_requests_per_cycle: middleware_limit_config
                                    .max_requests_per_cycle,
                                cycle_duration: middleware_limit_config.cycle_duration,
                            },
                        )
                        .await;

                    (false, result)
                }
                _ => (
                    true,
                    Ok(LimiterHeaders {
                        limit: 0,
                        remaining: 0,
                        reset: 0,
                    }),
                ),
            };

            if pass {
                return Ok(service.call(req).await?.map_into_left_body());
            }

            match limiter_result {
                Ok(headers) => {
                    req.extensions_mut()
                        .insert::<LimiterHeaders>(headers.clone());

                    let res = service.call(req).await?;
                    let (req, mut res) = res.into_parts();

                    let limit_header_value = HeaderValue::from_str(&headers.limit.to_string())
                        .unwrap_or(HeaderValue::from_static("0"));

                    let remaining_header_value = HeaderValue::from_str(&headers.remaining.to_string())
                        .unwrap_or(HeaderValue::from_static("0"));

                    let reset_header_value = HeaderValue::from_str(&headers.reset.to_string())
                        .unwrap_or(HeaderValue::from_static("0"));

                    let headers = res.headers_mut();

                    headers.insert(
                        http::header::HeaderName::from_static("x-ratelimit-limit"),
                        limit_header_value,
                    );
                    headers.insert(
                        http::header::HeaderName::from_static("x-ratelimit-remaining"),
                        remaining_header_value,
                    );
                    headers.insert(
                        http::header::HeaderName::from_static("x-ratelimit-reset"),
                        reset_header_value,
                    );

                    Ok(ServiceResponse::new(req, res).map_into_left_body())
                }
                Err(err) => {
                    let too_much_requests_res = HttpResponse::TooManyRequests().finish();
                    match err {
                        LimiterError::Limited=> Ok(req.into_response(too_much_requests_res).map_into_right_body()),
                        LimiterError::MemoryLimitExceeded => Ok(req.into_response(too_much_requests_res).map_into_right_body()),
                        LimiterError::RedisMemoryExceeded => Ok(req.into_response(too_much_requests_res).map_into_right_body()),
                        LimiterError::BothMemoryAndRedisMemoryExceeded => Ok(req.into_response(too_much_requests_res).map_into_right_body()),
                        _ => {
                            let res = HttpResponse::InternalServerError().finish();
                            Ok(req.into_response(res).map_into_right_body())
                        }
                    }
                }
            }
        })
    }
}
