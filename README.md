# Nervio Limiter

The agnostic rate limiter for the modern and fast web.

## Why?

Because i want

## Examples as Middleware

### Currently Supported Built In Middlewares
Feel free to build your own middleware, and with your custom logic. Check for `src/middlewares` for examples for available frameworks.

Automically middlewares set `x-ratelimit-remaining`, `x-ratelimit-limit` and `x-ratelimit-reset`, but `limiter.limit_this()` used as base of the middleware return more, `x-ratelimit-bucket` and `x-ratelimit-key` headers that can be set. But for simplicity only the first three are set.

* Actix Web

### `Limiter::limit_this`

This functions is the golden chicken, is the way to rate limit, and its used in the middlewares, you have to provide a entity_key for example, an account ID, an IP, or whatever, that is the entity or object to rate limit, and a bucket config where this entity will obey.

```rust
        // Example ID to Limit
        let ip = "127.0.0.1";
        let limiter = nervio_limiter::limiter::Limiter::new(...);

        // This can be put example in a middleware, route or what you need...
        match limiter
            .limit_this(
                ip.to_owned(),
                BucketConfig {
                    // Bucket Name
                    name: "my.global.bucket".to_owned(),
                    // Limit Entity Type
                    limit_by: LimitEntityType::IP,
                    // Maximum Requests per Cycle
                    max_requests_per_cycle: 120,
                    // And the cycle Duration
                    cycle_duration: Duration::from_secs(3600), // 1 Hour
                    // This will result in a bucket that rate limit at 120 Requests per Hour per entity (in this case an IP)
                },
            )
            .await
        {
            Ok(limiter_headers) => {
                // Do what you need with this headers, usually put in the response
            }
            Err(err) => {
                // Errors, including be limited

                match err {
                    LimiterError::Limited => {
                        // return a too_much_requests response
                    }
                    LimiterError::MemoryLimitExceeded => {
                        // return a too_much_requests response
                    }
                    LimiterError::RedisMemoryExceeded => {
                        // return a too_much_requests response
                    }
                    LimiterError::BothMemoryAndRedisMemoryExceeded => {
                        // return a too_much_requests response
                    }
                    _ => {
                        // Serious errors not related to limits
                    }
                }
            }
        };
```

### `Limiter::limit_this` response is a set of headers as LimiterHeaders

```rust
#[derive(Debug, Clone)]
pub struct LimiterHeaders {
    pub key: String,
    pub bucket: String,
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
}
```

* `x-ratelimit-remaining`: Remaining available requests
* `x-ratelimit-limit`: Initial limit supply
* `x-ratelimit-reset`: Normally this is a *from now when will expire* but i dont like it because isn't precise, so i return a UNIX timestamp in millseconds.
* `x-ratelimit-bucket`: Bucket Name
* `x-ratelimit-key`: The internal key to track the rate limit for requests

**NOTE** ActixWebLimiterMiddleware, is only available behind `actix-web` feature flag

## Actix Web
```rust
use std::{sync::Arc, time::Duration};
use actix_web::{App, HttpServer};
use nervio_limiter::{limiter::{BucketConfig, Limiter}, middleware::actix_web::ActixWebLimiterMiddleware, storage::StorageType};
use tokio::sync::Mutex;

pub fn init_server() {
    let limiter = Limiter::new(
        StorageType::InMemory, // Storage Type (InMemory, Redis or RedisAndMemoryMix)
        None, // MultiplexedConnection Redis Connection
        Some(200), // InMemory Max Allowed (In MB)
        None, // Redis Memory Max Allowed (In MB)
        Duration::from_secs(10), // This option only works with RedisAndMemoryMix Storage Type, and means the time that a InMemory item can be in memory  without accesing to it, before deleting it but keeps in Redis Memory, its like a cache for the cache
    );

    let limiter_middleware = ActixWebLimiterMiddleware {
        limiter: Arc::new(Mutex::new(limiter)), // Allow use the same limiter for multiple stuff, so just pass the pointer as mutex to
        middleware_bucket_config: BucketConfig {
            name: "limiter".to_owned(), // Bucker Name, used in keys, treat it like a id, recommended to keep it unique, or use it across multiples midlewares or routes the same rate limit
            limit_by: nervio_limiter::limiter::LimitEntityType::Global, // Select the method to limit requests, global means a global limit for everyone, also exists others like IP, only Global and IP can be used for now for this middleware
            max_requests_per_cycle: 1, // Max Request For Cycle
            cycle_duration: Duration::from_secs(10), // Each cycle duration, when a cycle ends, the requests are refilled
        },
    };

    let server = HttpServer::new(move || {
        App::new()
            .wrap(limiter_middleware.clone()) // Clone our LimiterMiddleware
    });
}
```

## Or do you want a more granular rate limiter? For example per route

```rust
use std::time::Duration;

use nervio_limiter::{errors::LimiterError, limiter::{self, BucketConfig}};
use actix_web::{HttpMessage, HttpRequest, Responder, Result};

pub async fn controller(
    req: HttpRequest,
    state: MyExampleState,
) -> Result<impl Responder> {
    // In real cases for example me i would put the limiter in state, then we need to lock, and then we can use the limiter
    let limiter_headers = state.limiter.lock().await.limit_this("_".to_owned(), BucketConfig {
        name: "my_controller".to_string(), // This is the name of the limiter, use the same name to used across multiples routes the same limiter
        limit_by: limiter::LimitEntityType::Global, // This is the type of the limiter, in this case is global
        max_requests_per_cycle: 120, // This is the max requests per cycle
        cycle_duration: Duration::from_secs(60), // This is the cycle duration
    }).await.map_err(|err| {
        let too_much_requests_res = actix_web::error::ErrorTooManyRequests("Too many requests".to_string());
        match err {
            LimiterError::Limited=> too_much_requests_res,
            LimiterError::MemoryLimitExceeded => too_much_requests_res,
            LimiterError::RedisMemoryExceeded => too_much_requests_res,
            LimiterError::BothMemoryAndRedisMemoryExceeded => too_much_requests_res,
            _ => actix_web::error::ErrorInternalServerError(err) // Other error not related to limits
        }
    })?;

    // Your other logic, you can optionally insert headers to the res
    ...
}
```

## Contributions

Feel free to contribute to this project :)

## Community

(there aren't really a community, its just me) [https://discord.gg/xtBskESBnY](https://discord.gg/xtBskESBnY)
