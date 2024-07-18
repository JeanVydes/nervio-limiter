# Nervio Limiter

The agnostic rate limiter for the modern and fast web.

## Why?

Because i want

## Examples as Middleware

### Currently Supported Built In Middlewares
Feel free to build your own middleware, and with your custom logic. Check for `src/middlewares` for examples for available frameworks

* Actix Web

## Actix Web
```rust
use std::{sync::Arc, time::Duration};

use actix_web::{App, HttpServer};
use nervio_limiter::{limiter::{LimitThisConfig, Limiter}, middleware::actix_web::ActixWebLimiterMiddleware, storage::StorageType};
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
        middleware_limit_config: LimitThisConfig {
            name: "limiter".to_owned(), // Limiter Name, used in keys, recommended to keep it unique
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

use nervio_limiter::{errors::LimiterError, limiter::{self, LimitThisConfig}};
use actix_web::{HttpMessage, HttpRequest, Responder, Result};

pub async fn controller(
    req: HttpRequest,
    state: MyExampleState,
) -> Result<impl Responder> {
    // In real cases for example me i would put the limiter in state, then we need to lock, and then we can use the limiter
    let limiter_headers = state.limiter.lock().await.limit_this("_".to_owned(), LimitThisConfig {
        name: "validate_token".to_string(), // This is the name of the limiter
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
