use derive_more::Display; // Add derive_more dependency
use serde::Deserialize;

#[derive(Debug, Display, Clone, Deserialize)] // Added Display derive
pub enum LimiterError {
    #[display("Storage type is not configured")]
    NotStorageConfigured,
    #[display("Storage error")]
    StorageError,

    #[display("Rate limit error")]
    RateLimitError,
    #[display("Unknown error")]
    UnknownError,

    #[display("Failed to serialize data")]
    SerializationError,
    #[display("Failed to deserialize data")]
    DeserializationError,

    #[display("In-memory storage limit exceeded")]
    MemoryLimitExceeded,
    #[display("Redis storage limit exceeded")]
    RedisMemoryExceeded,
    #[display("Both in-memory and Redis storage limits exceeded")]
    BothMemoryAndRedisMemoryExceeded,

    #[display("Redis storage is not configured")]
    NotRedisConfigured,
    #[display("Failed to execute Redis SET command")]
    RedisSetError,
    #[display("Failed to execute Redis GET command")]
    RedisGetError,
    #[display("Failed to execute Redis DEL command")]
    RedisDelError,
    #[display("Failed to get or parse Redis memory usage info")]
    RedisGetUsageMemoryError,
    #[display("No IP found in request")]
    NoIPFound,

    #[display("Rate limit exceeded")]
    Limited,
}

impl std::error::Error for LimiterError {}
