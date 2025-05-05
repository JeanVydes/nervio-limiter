use hashbrown::HashMap;
use redis::aio::MultiplexedConnection;
use std::time::Duration;

use crate::errors::LimiterError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorageType {
    Redis,
    InMemory,
    RedisAndMemoryMix,
}

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub storage_type: Option<StorageType>,
    pub redis_conn: Option<MultiplexedConnection>,

    pub max_memory_size: Option<u64>, // in MB
    pub max_redis_size: Option<u64>,  // in MB

    pub acceptable_last_accesed_time_to_cache_redis_in_memory: Option<Duration>, // in seconds
}

pub async fn get_redis_info(
    redis_conn: &mut MultiplexedConnection,
) -> Result<HashMap<String, String>, LimiterError> {
    // Execute the INFO MEMORY command
    let info: String = match redis::cmd("INFO")
        .query_async::<MultiplexedConnection, String>(&mut redis_conn.clone())
        .await
    {
        Ok(info) => info,
        Err(_) => return Err(LimiterError::RedisSetError),
    };

    // Parse the info string to extract used_memory
    let info = info
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() == 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect::<HashMap<String, String>>();

    return Ok(info);
}

pub async fn get_redis_memory_usage(
    redis_conn: &mut MultiplexedConnection,
) -> Result<usize, LimiterError> {
    let info = get_redis_info(redis_conn).await?;
    let memory_usage = info
        .get("used_memory")
        .map_or(Err(LimiterError::RedisGetUsageMemoryError), |v| {
            Ok(v.to_string())
        })?;
    let memory_usage = memory_usage
        .parse::<usize>()
        .map_err(|_| LimiterError::RedisGetUsageMemoryError)?;
    Ok(memory_usage)
}
