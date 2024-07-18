use std::{sync::Arc, time::Duration};

use hashbrown::HashMap;
use log::{error, info};
use redis::aio::MultiplexedConnection;
use serde::{de, Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    errors::LimiterError,
    storage::{get_hashmap_memory_size, get_redis_memory_usage, StorageConfig, StorageType},
};

#[derive(Debug, Clone)]
pub struct Limiter {
    pub storage_type: StorageType,

    pub redis_conn: Option<MultiplexedConnection>,

    pub in_memory: Arc<Mutex<HashMap<String, Entity>>>,

    pub max_memory_size: Option<u64>, // in MB
    pub max_redis_size: Option<u64>,  // in MB

    pub acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,

    pub current_redis_memory_usage: Option<Arc<Mutex<u64>>>,
    pub last_fetched_redis_memory_usage: Option<u64>,

    pub current_memory_usage: Option<Arc<Mutex<u64>>>,
    pub last_fetched_memory_usage: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum LimitEntityType {
    Global,
    IP,
    AccountID,
    Custom(String),
}

#[derive(Debug, Clone)]
pub struct LimitThisConfig {
    pub name: String,
    pub limit_by: LimitEntityType,

    pub max_requests_per_cycle: u64,
    pub cycle_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct LimiterHeaders {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub entity_type: LimitEntityType,
    pub initial_supply: u64,
    pub remaining: u64,
    pub created_at: u64,
    pub expires_at: u64,
    pub last_accessed_at: u64,
}

impl Limiter {
    pub fn new(
        storage_type: StorageType,
        redis_conn: Option<MultiplexedConnection>,
        max_memory_size: Option<u64>,
        max_redis_size: Option<u64>,
        acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,
    ) -> Self {
        info!("Creating new limiter with storage type: {:?}", storage_type);
        Limiter {
            storage_type,
            redis_conn,
            in_memory: Arc::new(Mutex::new(HashMap::new())),
            max_memory_size,
            max_redis_size,

            acceptable_last_accesed_time_to_cache_redis_in_memory,

            current_redis_memory_usage: None,
            last_fetched_redis_memory_usage: None,

            current_memory_usage: None,
            last_fetched_memory_usage: None,
        }
    }

    pub fn builder() -> LimiterBuilder {
        LimiterBuilder::default()
    }

    pub async fn add_to_redis<T>(
        &mut self,
        key: String,
        value: T,
        duration: Duration,
    ) -> Result<(), LimiterError>
    where
        T: Serialize + Clone,
    {
        let redis_conn = match self.redis_conn {
            Some(ref conn) => conn,
            None => return Err(LimiterError::NotRedisConfigured),
        };

        let exp = duration.as_millis() as u64;
        let parsed_value =
            serde_json::to_string(&value).map_err(|_| LimiterError::SerializationError)?;

        let _ = redis::cmd("SET")
            .arg(&key)
            .arg(parsed_value)
            .query_async::<MultiplexedConnection, ()>(&mut redis_conn.clone())
            .await
            .map_err(|_| return Err::<T, LimiterError>(LimiterError::RedisSetError));

        // Then, set the expiration
        match redis::cmd("PEXPIRE")
            .arg(&key)
            .arg(exp)
            .query_async::<MultiplexedConnection, ()>(&mut redis_conn.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(_) => Err(LimiterError::RedisSetError),
        }
    }

    pub async fn get_from_redis<T>(&mut self, key: String) -> Result<Option<T>, LimiterError>
    where
        T: de::DeserializeOwned,
    {
        let redis_conn = match self.redis_conn {
            Some(ref conn) => conn,
            None => return Err(LimiterError::NotRedisConfigured),
        };

        let value: Option<String> = match redis::cmd("GET")
            .arg(&key)
            .query_async::<MultiplexedConnection, Option<String>>(&mut redis_conn.clone())
            .await
        {
            Ok(v) => v,
            Err(_) => return Err(LimiterError::RedisSetError),
        };

        match value {
            Some(v) => match serde_json::from_str(&v) {
                Ok(parsed_value) => Ok(Some(parsed_value)),
                Err(_) => Err(LimiterError::DeserializationError),
            },
            None => Ok(None),
        }
    }

    pub async fn delete_from_redis(&mut self, key: String) -> Result<(), LimiterError> {
        let redis_conn = match self.redis_conn {
            Some(ref conn) => conn,
            None => return Err(LimiterError::NotRedisConfigured),
        };

        match redis::cmd("DEL")
            .arg(&key)
            .query_async::<MultiplexedConnection, ()>(&mut redis_conn.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(_) => Err(LimiterError::RedisSetError),
        }
    }

    pub async fn limit_this(
        &mut self,
        mut entity_key: String,
        config: LimitThisConfig,
    ) -> Result<LimiterHeaders, LimiterError> {
        let key_prefix = match config.limit_by {
            LimitEntityType::Global => "global",
            LimitEntityType::IP => "ip",
            LimitEntityType::AccountID => "account_id",
            LimitEntityType::Custom(ref custom_key) => custom_key,
        };

        if config.limit_by == LimitEntityType::Global {
            entity_key = "_".to_string();
        }

        let key = format!("{}:{}:{}", config.name, key_prefix, entity_key);
        let entity = self
            .get_entity(
                key.clone(),
                self.acceptable_last_accesed_time_to_cache_redis_in_memory,
            )
            .await?;
        let now = chrono::Utc::now().timestamp_millis() as u64;

        if let Some(mut entity) = entity {
            let can_reset = now > entity.expires_at;

            // If expired, then create a new one
            if can_reset {
                entity = self
                    .reset_entity(key.clone(), entity.entity_type, config)
                    .await?;
            } else {
                // Not expired yet
                let mut limited = false;

                // Check if entity is limited
                if entity.remaining == 0 {
                    limited = true;
                }

                // If limited, return error
                if limited {
                    return Err(LimiterError::Limited);
                }

                // Decrease the remaining count and update the entity
                entity = self
                    .decrease_remaining_and_update(key, entity.clone(), config)
                    .await?;
            }

            return Ok(LimiterHeaders {
                limit: entity.initial_supply,
                remaining: entity.remaining,
                reset: entity.expires_at,
            });
        }

        let _ = self.check_and_update_memory_usage().await;

        // If entity is not found, the create a new one
        let expires_at = now + config.cycle_duration.as_millis() as u64;
        let new_entity = Entity {
            entity_type: config.limit_by,
            initial_supply: config.max_requests_per_cycle,
            remaining: config.max_requests_per_cycle,
            created_at: now,
            expires_at,
            last_accessed_at: now,
        };

        // Save the new entity
        if self.storage_type == StorageType::Redis {
            if let Some(max_memory_size) = self.max_redis_size {
                if let Some(current_memory_usage) = &self.current_redis_memory_usage {
                    let current_memory_usage = current_memory_usage.lock().await;
                    let size_in_mb = *current_memory_usage as f64 / (1024.0 * 1024.0);
                    if size_in_mb > max_memory_size as f64 {
                        return Err(LimiterError::RedisMemoryExceeded);
                    }
                }
            }

            self.add_to_redis(key.clone(), new_entity.clone(), config.cycle_duration)
                .await?
        } else if self.storage_type == StorageType::InMemory {
            // Check if max_memory_size is provide and then check against hashmap size
            if let Some(max_memory_size) = self.max_memory_size {
                if let Some(current_memory_usage) = &self.current_memory_usage {
                    let current_memory_usage = current_memory_usage.lock().await;

                    let size_in_mb = *current_memory_usage as f64 / (1024.0 * 1024.0);

                    if size_in_mb > max_memory_size as f64 {
                        return Err(LimiterError::MemoryLimitExceeded);
                    }
                }
            }

            self.in_memory
                .lock()
                .await
                .insert(key.clone(), new_entity.clone());
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            let mut memory_excceded = false;
            let mut redis_memory_excceded = false;

            if let Some(max_memory_size) = self.max_memory_size {
                // Passing the pointer to the in-memory hashmap
                let size_in_bytes =
                    get_hashmap_memory_size::<String, Entity>(self.in_memory.clone()).await;
                let size_in_mb = size_in_bytes as f64 / (1024.0 * 1024.0);

                if size_in_mb > max_memory_size as f64 {
                    memory_excceded = true;
                }
            }

            if let Some(max_memory_size) = self.max_redis_size {
                if let Some(current_memory_usage) = &self.current_redis_memory_usage {
                    let current_memory_usage = current_memory_usage.lock().await;
                    let size_in_mb = *current_memory_usage as f64 / (1024.0 * 1024.0);
                    if size_in_mb > max_memory_size as f64 {
                        redis_memory_excceded = true;
                    }
                }
            }

            if memory_excceded && redis_memory_excceded {
                return Err(LimiterError::BothMemoryAndRedisMemoryExceeded);
            }

            if !memory_excceded {
                self.in_memory
                    .lock()
                    .await
                    .insert(key.clone(), new_entity.clone());
            }

            if !redis_memory_excceded {
                self.add_to_redis(key.clone(), new_entity.clone(), config.cycle_duration)
                    .await?
            }
        }

        Ok(LimiterHeaders {
            limit: new_entity.initial_supply,
            remaining: new_entity.remaining,
            reset: new_entity.expires_at,
        })
    }

    pub async fn get_entity(
        &mut self,
        key: String,
        acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,
    ) -> Result<Option<Entity>, LimiterError> {
        let mut entity: Option<Entity>;
        if self.storage_type == StorageType::Redis {
            entity = self.get_from_redis(key.clone()).await?;
        } else if self.storage_type == StorageType::InMemory {
            entity = self.in_memory.lock().await.get(&key).cloned();
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            entity = self.in_memory.lock().await.get(&key).cloned();

            let now = chrono::Utc::now().timestamp_millis() as u64;

            if entity.is_none() {
                entity = self.get_from_redis(key.clone()).await?;

                // if exists update local memory
                if let Some(entity) = entity.clone() {
                    let was_accesed_in_the_last_acceptable_time = (now - entity.last_accessed_at)
                        < acceptable_last_accesed_time_to_cache_redis_in_memory.as_millis() as u64;

                    if now < entity.expires_at && was_accesed_in_the_last_acceptable_time {
                        self.in_memory
                            .lock()
                            .await
                            .insert(key.clone(), entity.clone());
                    }
                }
            }
        } else {
            return Err(LimiterError::NotStorageConfigured);
        }

        Ok(entity)
    }

    pub async fn reset_entity(
        &mut self,
        key: String,
        entity_type: LimitEntityType,
        config: LimitThisConfig,
    ) -> Result<Entity, LimiterError> {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let expires_at = now + config.cycle_duration.as_millis() as u64;

        let entity = Entity {
            entity_type,
            initial_supply: config.max_requests_per_cycle,
            remaining: config.max_requests_per_cycle,
            created_at: now,
            expires_at,
            last_accessed_at: now,
        };

        if self.storage_type == StorageType::Redis {
            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        } else if self.storage_type == StorageType::InMemory {
            self.in_memory
                .lock()
                .await
                .insert(key.clone(), entity.clone());
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            self.in_memory
                .lock()
                .await
                .insert(key.clone(), entity.clone());

            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        }

        Ok(entity)
    }

    pub async fn decrease_remaining_and_update(
        &mut self,
        key: String,
        mut entity: Entity,
        config: LimitThisConfig,
    ) -> Result<Entity, LimiterError> {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        if self.storage_type == StorageType::Redis && entity.remaining > 0 {
            entity.remaining -= 1;
            entity.last_accessed_at = now;

            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        } else if self.storage_type == StorageType::InMemory && entity.remaining > 0 {
            entity.remaining -= 1;
            entity.last_accessed_at = now;

            self.in_memory
                .lock()
                .await
                .insert(key.clone(), entity.clone());
        } else if self.storage_type == StorageType::RedisAndMemoryMix && entity.remaining > 0 {
            let not_too_much_demanded = (now - entity.last_accessed_at)
                > self
                    .acceptable_last_accesed_time_to_cache_redis_in_memory
                    .as_millis() as u64;

            if not_too_much_demanded {
                self.in_memory.lock().await.remove(&key);
            } else {
                entity.remaining -= 1;
                self.in_memory
                    .lock()
                    .await
                    .insert(key.clone(), entity.clone());
            }

            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        }

        Ok(entity)
    }

    pub async fn check_and_update_memory_usage(&mut self) -> Result<(), LimiterError> {
        if self.storage_type == StorageType::Redis {
            if let Some(max_redis_size) = self.max_redis_size {
                // If max_redis_size its not provided, then it's not necessary to check the memory usage
                // But the recommended is to always provide the max_redis_size

                let last_fetched_redis_memory_usage =
                    self.last_fetched_redis_memory_usage.unwrap_or(0);

                let now = chrono::Utc::now().timestamp_millis() as u64;
                let acceptable_time_to_fetch_redis_memory_usage = Duration::from_secs(5);

                let can_fetch_redis_memory_usage = (now - last_fetched_redis_memory_usage)
                    > acceptable_time_to_fetch_redis_memory_usage.as_millis() as u64;

                // Check if it's time to fetch the memory usage
                if can_fetch_redis_memory_usage {
                    let redis_conn = match &self.redis_conn {
                        Some(conn) => conn,
                        None => return Err(LimiterError::NotRedisConfigured),
                    };

                    let redis_memory_usage =
                        get_redis_memory_usage(&mut redis_conn.clone()).await?;
                    let redis_memory_usage_mb = redis_memory_usage as f64 / (1024.0 * 1024.0);

                    info!(
                        "Redis Memory Usage: {:?}MB/{:?}MB",
                        redis_memory_usage_mb, max_redis_size
                    );
                    if redis_memory_usage_mb > max_redis_size as f64 {
                        error!(
                            "REDIS MEMORY HAS REACHED THE LIMIT {:?}MB/{:?}MB",
                            redis_memory_usage_mb, max_redis_size
                        );
                    }

                    if let Some(current_redis_memory_usage_mb) = &self.current_redis_memory_usage {
                        let mut current_redis_memory_usage_mb =
                            current_redis_memory_usage_mb.lock().await;

                        *current_redis_memory_usage_mb = redis_memory_usage_mb as u64;
                    } else {
                        self.current_redis_memory_usage =
                            Some(Arc::new(Mutex::new(redis_memory_usage_mb as u64)));
                    }

                    self.last_fetched_redis_memory_usage =
                        Some(chrono::Utc::now().timestamp_millis() as u64);
                }
            }
        }

        if self.storage_type == StorageType::RedisAndMemoryMix
            || self.storage_type == StorageType::InMemory
        {
            let last_fetched_memory_usage = self.last_fetched_memory_usage.unwrap_or(0);

            let now = chrono::Utc::now().timestamp_millis() as u64;
            let acceptable_time_to_fetch_memory_usage = Duration::from_secs(5);

            let can_fetch_memory_usage = (now - last_fetched_memory_usage)
                > acceptable_time_to_fetch_memory_usage.as_millis() as u64;

            // Check if it's time to fetch the memory usage
            if can_fetch_memory_usage {
                let memory_usage =
                    get_hashmap_memory_size::<String, Entity>(self.in_memory.clone()).await;
                let memory_usage_mb = memory_usage as f64 / (1024.0 * 1024.0);

                if let Some(current_memory_usage_mb) = &self.current_memory_usage {
                    let mut current_memory_usage_mb = current_memory_usage_mb.lock().await;

                    *current_memory_usage_mb = memory_usage_mb as u64;
                } else {
                    self.current_memory_usage = Some(Arc::new(Mutex::new(memory_usage_mb as u64)));
                }

                self.last_fetched_memory_usage = Some(chrono::Utc::now().timestamp_millis() as u64);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct LimiterBuilder {
    pub storage_config: StorageConfig,
}

impl LimiterBuilder {
    pub fn default() -> Self {
        LimiterBuilder {
            storage_config: StorageConfig {
                storage_type: None,
                redis_conn: None,
                max_memory_size: None,
                max_redis_size: None,
                acceptable_last_accesed_time_to_cache_redis_in_memory: None,
            },
        }
    }

    pub fn set_storage_type(mut self, storage_type: StorageType) -> Self {
        self.storage_config.storage_type = Some(storage_type);
        self
    }

    pub fn set_redis_conn(mut self, redis_conn: MultiplexedConnection) -> Self {
        self.storage_config.redis_conn = Some(redis_conn);
        self
    }

    pub fn set_max_memory_size(mut self, max_memory_size: u64) -> Self {
        self.storage_config.max_memory_size = Some(max_memory_size);
        self
    }

    pub fn set_max_redis_size(mut self, max_redis_size: u64) -> Self {
        self.storage_config.max_redis_size = Some(max_redis_size);
        self
    }

    pub fn set_acceptable_last_accesed_time_to_cache_redis_in_memory(
        mut self,
        acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,
    ) -> Self {
        self.storage_config
            .acceptable_last_accesed_time_to_cache_redis_in_memory =
            Some(acceptable_last_accesed_time_to_cache_redis_in_memory);
        self
    }

    pub fn build(self) -> Limiter {
        Limiter {
            storage_type: self.storage_config.storage_type.unwrap(),
            redis_conn: self.storage_config.redis_conn,
            in_memory: Arc::new(Mutex::new(HashMap::new())),
            max_memory_size: self.storage_config.max_memory_size,
            max_redis_size: self.storage_config.max_redis_size,

            acceptable_last_accesed_time_to_cache_redis_in_memory: self
                .storage_config
                .acceptable_last_accesed_time_to_cache_redis_in_memory
                .unwrap_or(Duration::from_secs(5)),

            current_redis_memory_usage: None,
            last_fetched_redis_memory_usage: None,

            current_memory_usage: None,
            last_fetched_memory_usage: None,
        }
    }
}
