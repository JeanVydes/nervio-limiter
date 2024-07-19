use core::fmt;
use std::fmt::{Display, Formatter};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LimiterError {
    NotStorageConfigured,
    StorageError,

    RateLimitError,
    UnknownError,

    SerializationError,
    DeserializationError,

    MemoryLimitExceeded,
    RedisMemoryExceeded,
    BothMemoryAndRedisMemoryExceeded,

    NotRedisConfigured,
    RedisSetError,
    RedisGetUsageMemoryError,

    NoIPFound,

    Limited,
}

impl Display for LimiterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}