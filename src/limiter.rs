use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use dashmap::DashMap;
use log::{debug, error, info, warn};
use redis::aio::MultiplexedConnection;
use serde::{de, Deserialize, Serialize};

use crate::{
    errors::LimiterError,
    storage::{get_redis_memory_usage, StorageConfig, StorageType},
};

#[derive(Debug)]
pub struct Limiter {
    pub storage_type: StorageType,

    pub redis_conn: Option<MultiplexedConnection>,

    pub in_memory: Arc<DashMap<String, Entity>>,

    pub max_memory_size: Option<u64>, // in MB
    pub max_redis_size: Option<u64>,  // in MB

    pub acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,

    // These are updated periodically by a background task
    pub current_redis_memory_usage_mb: Arc<AtomicU64>,
    pub current_memory_usage_mb: Arc<AtomicU64>,

    // Handle for the background task
    _memory_check_handle: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum LimitEntityType {
    Global,
    IP,        // For unproxied IP
    ProxiedIP, // For services that are behind a proxy
    ID,
    Custom(String),
}

#[derive(Debug, Clone)]
pub struct BucketConfig {
    pub name: String,
    pub limit_by: LimitEntityType,
    pub max_requests_per_cycle: u64,
    pub cycle_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct LimiterHeaders {
    pub key: String,
    pub bucket: String,
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
            in_memory: Arc::new(DashMap::new()),
            max_memory_size,
            max_redis_size,

            acceptable_last_accesed_time_to_cache_redis_in_memory,

            current_redis_memory_usage_mb: Arc::new(AtomicU64::new(0)),
            current_memory_usage_mb: Arc::new(AtomicU64::new(0)),

            // Background task handle would be set up later, perhaps in `build` or a dedicated start method
            _memory_check_handle: None,
        }
    }

    pub fn builder() -> LimiterBuilder {
        LimiterBuilder::default()
    }

    pub async fn add_to_redis<T>(
        &self,
        key: String,
        value: T,
        duration: Duration,
    ) -> Result<(), LimiterError>
    where
        T: Serialize + Clone,
    {
        let redis_conn = match self.redis_conn {
            // Clone the connection
            Some(ref conn) => conn.clone(),
            None => return Err(LimiterError::NotRedisConfigured),
        };

        let exp_ms = duration.as_millis(); // Keep as u128 for precision if needed, but usize for command
        let parsed_value =
            serde_json::to_string(&value).map_err(|_| LimiterError::SerializationError)?;

        // Use SET with PX for atomic set + expire
        match redis::cmd("SET")
            .arg(&key)
            .arg(parsed_value)
            .arg("PX")
            .arg(exp_ms as usize) // PX expects milliseconds as integer
            .query_async::<MultiplexedConnection, ()>(&mut redis_conn.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(_) => Err(LimiterError::RedisSetError),
        }
    }

    pub async fn get_from_redis<T>(&self, key: String) -> Result<Option<T>, LimiterError>
    where
        T: de::DeserializeOwned,
    {
        let mut redis_conn = match self.redis_conn {
            // Clone the connection
            Some(ref conn) => conn.clone(),
            None => return Err(LimiterError::NotRedisConfigured),
        };

        let value: Option<String> = match redis::cmd("GET")
            .arg(&key)
            .query_async::<MultiplexedConnection, Option<String>>(&mut redis_conn)
            .await
        {
            Ok(v) => v,
            Err(_) => return Err(LimiterError::RedisGetError), // Use more specific error
        };

        match value {
            Some(v) => match serde_json::from_str(&v) {
                Ok(parsed_value) => Ok(Some(parsed_value)),
                Err(_) => Err(LimiterError::DeserializationError),
            },
            None => Ok(None),
        }
    }

    pub async fn delete_from_redis(&self, key: String) -> Result<(), LimiterError> {
        let mut redis_conn = match self.redis_conn {
            // Clone the connection
            Some(ref conn) => conn.clone(),
            None => return Err(LimiterError::NotRedisConfigured),
        };

        match redis::cmd("DEL")
            .arg(&key)
            .query_async::<MultiplexedConnection, ()>(&mut redis_conn)
            .await // Removed clone here
        {
            Ok(_) => Ok(()),
            Err(_) => Err(LimiterError::RedisDelError), // Use more specific error
        }
    }

    pub async fn limit_this(
        &self, // Changed to &self
        mut entity_key: String,
        config: &BucketConfig,
    ) -> Result<LimiterHeaders, LimiterError> {
        let key_prefix = match config.limit_by {
            LimitEntityType::Global => "global",
            LimitEntityType::IP => "ip",
            LimitEntityType::ProxiedIP => "proxied_ip",
            LimitEntityType::ID => "id",
            LimitEntityType::Custom(ref custom_key) => custom_key,
        };

        if config.limit_by == LimitEntityType::Global {
            entity_key = "_".to_string();
        }

        let key = format!("{}:{}:{}", config.name.clone(), key_prefix, entity_key);
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
                    .reset_entity(key.clone(), entity.entity_type, &config)
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
                    .decrease_remaining_and_update(key.clone(), entity.clone(), &config)
                    .await?;
            }

            return Ok(LimiterHeaders {
                key,
                bucket: config.name.clone(),
                limit: entity.initial_supply,
                remaining: entity.remaining,
                reset: entity.expires_at,
            });
        }

        // If entity is not found, the create a new one
        let expires_at = now + config.cycle_duration.as_millis() as u64;
        let new_entity = Entity {
            entity_type: config.limit_by.clone(),
            initial_supply: config.max_requests_per_cycle,
            remaining: config.max_requests_per_cycle,
            created_at: now,
            expires_at,
            last_accessed_at: now,
        };

        // Save the new entity
        if self.storage_type == StorageType::Redis {
            // Check pre-calculated memory usage
            if let Some(max_memory_size) = self.max_redis_size {
                if self.current_memory_usage_mb.load(Ordering::SeqCst) > max_memory_size {
                    return Err(LimiterError::RedisMemoryExceeded);
                }
            }

            self.add_to_redis(key.clone(), new_entity.clone(), config.cycle_duration)
                .await?
        } else if self.storage_type == StorageType::InMemory {
            // Check pre-calculated memory usage
            if let Some(max_memory_size) = self.max_memory_size {
                if self.current_memory_usage_mb.load(Ordering::SeqCst) > max_memory_size {
                    return Err(LimiterError::MemoryLimitExceeded);
                }
            }

            self.in_memory.insert(key.clone(), new_entity.clone()); // Use DashMap insert
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            let mut memory_excceded = false;
            let mut redis_memory_excceded = false;

            if let Some(max_memory_size) = self.max_memory_size {
                if self.current_memory_usage_mb.load(Ordering::SeqCst) > max_memory_size {
                    memory_excceded = true;
                }
            }

            if let Some(max_memory_size) = self.max_redis_size {
                if self.current_memory_usage_mb.load(Ordering::SeqCst) > max_memory_size {
                    redis_memory_excceded = true;
                }
            }

            if memory_excceded && redis_memory_excceded {
                return Err(LimiterError::BothMemoryAndRedisMemoryExceeded);
            }

            if !memory_excceded {
                self.in_memory.insert(key.clone(), new_entity.clone()); // Use DashMap insert
            }

            if !redis_memory_excceded {
                self.add_to_redis(key.clone(), new_entity.clone(), config.cycle_duration)
                    .await?
            }
        }

        Ok(LimiterHeaders {
            key: key.to_owned(),
            bucket: config.name.clone(),
            limit: new_entity.initial_supply,
            remaining: new_entity.remaining,
            reset: new_entity.expires_at,
        })
    }

    pub async fn get_entity(
        &self, // Changed to &self
        key: String,
        acceptable_last_accesed_time_to_cache_redis_in_memory: Duration,
    ) -> Result<Option<Entity>, LimiterError> {
        let mut entity: Option<Entity>;
        if self.storage_type == StorageType::Redis {
            entity = self.get_from_redis(key.clone()).await?;
        } else if self.storage_type == StorageType::InMemory {
            entity = self.in_memory.get(&key).map(|e| e.value().clone()); // Use DashMap get
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            entity = self.in_memory.get(&key).map(|e| e.value().clone()); // Use DashMap get

            let now = chrono::Utc::now().timestamp_millis() as u64;

            if entity.is_none() {
                entity = self.get_from_redis(key.clone()).await?;

                // if exists update local memory
                if let Some(entity) = entity.clone() {
                    let was_accesed_in_the_last_acceptable_time = (now - entity.last_accessed_at)
                        < acceptable_last_accesed_time_to_cache_redis_in_memory.as_millis() as u64;

                    if now < entity.expires_at && was_accesed_in_the_last_acceptable_time {
                        self.in_memory.insert(key.clone(), entity.clone()); // Use DashMap insert
                    }
                }
            }
        } else {
            return Err(LimiterError::NotStorageConfigured);
        }

        Ok(entity)
    }

    pub async fn reset_entity(
        &self, // Changed to &self
        key: String,
        entity_type: LimitEntityType,
        config: &BucketConfig,
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
            self.in_memory.insert(key.clone(), entity.clone()); // Use DashMap insert
        } else if self.storage_type == StorageType::RedisAndMemoryMix {
            self.in_memory.insert(key.clone(), entity.clone()); // Use DashMap insert

            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        }

        Ok(entity)
    }

    pub async fn decrease_remaining_and_update(
        &self, // Changed to &self
        key: String,
        mut entity: Entity,
        config: &BucketConfig,
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

            self.in_memory.insert(key.clone(), entity.clone()); // Use DashMap insert
        } else if self.storage_type == StorageType::RedisAndMemoryMix && entity.remaining > 0 {
            let not_too_much_demanded = (now - entity.last_accessed_at)
                > self
                    .acceptable_last_accesed_time_to_cache_redis_in_memory
                    .as_millis() as u64;

            if not_too_much_demanded {
                self.in_memory.remove(&key); // Use DashMap remove
            } else {
                entity.remaining -= 1;
                self.in_memory.insert(key.clone(), entity.clone()); // Use DashMap insert
            }

            self.add_to_redis(key.clone(), entity.clone(), config.cycle_duration)
                .await?
        }

        Ok(entity)
    }

    // This should be spawned as a background task, e.g., in the `build` method or a dedicated `start` method.
    async fn memory_check_task(
        interval: Duration,
        storage_type: StorageType,
        redis_conn: Option<MultiplexedConnection>,
        in_memory_map: Option<Arc<DashMap<String, Entity>>>, // Changed type
        max_redis_size_mb: Option<u64>,
        max_memory_size_mb: Option<u64>,
        current_redis_mb: Arc<AtomicU64>,
        current_memory_mb: Arc<AtomicU64>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            debug!("Running periodic memory check...");

            // Check Redis Memory
            if (storage_type == StorageType::Redis
                || storage_type == StorageType::RedisAndMemoryMix)
                && redis_conn.is_some()
            {
                let mut conn = redis_conn.clone().unwrap(); // Safe unwrap due to check above
                match get_redis_memory_usage(&mut conn).await {
                    Ok(usage_bytes) => {
                        let usage_mb = usage_bytes as f64 / (1024.0 * 1024.0);
                        current_redis_mb.store(usage_mb as u64, Ordering::SeqCst);
                        debug!("Current Redis memory usage: {:.2} MB", usage_mb);
                        if let Some(max_mb) = max_redis_size_mb {
                            if usage_mb > max_mb as f64 {
                                error!(
                                    "REDIS MEMORY LIMIT EXCEEDED: {:.2}MB / {}MB",
                                    usage_mb, max_mb
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to get Redis memory usage: {:?}", e);
                        // Optionally clear the value or keep the stale one
                        // *current_redis_mb.lock().await = None;
                    }
                }
            }

            // Check In-Memory Map Size
            if (storage_type == StorageType::InMemory
                || storage_type == StorageType::RedisAndMemoryMix)
                && in_memory_map.is_some()
            {
                // Note: Calculating exact memory size for DashMap is complex.
                // We'll use len() * size_of entry as a rough estimate.
                // For a more accurate (but potentially slower) size, you'd need to iterate.
                let map_arc = in_memory_map.clone().unwrap(); // Safe unwrap
                let usage_bytes = get_dashmap_approx_memory_size(&map_arc); // Use new helper
                let usage_mb = usage_bytes as f64 / (1024.0 * 1024.0);
                current_memory_mb.store(usage_mb as u64, Ordering::SeqCst);
                debug!("Current In-Memory usage: {:.2} MB", usage_mb);
                if let Some(max_mb) = max_memory_size_mb {
                    if usage_mb > max_mb as f64 {
                        error!("IN-MEMORY LIMIT EXCEEDED: {:.2}MB / {}MB", usage_mb, max_mb);
                    }
                }
            }
        }
    }

    // Helper to start the background task
    pub fn start_memory_check_task(&mut self, interval: Duration) {
        // Prevent starting multiple tasks if called again
        if self._memory_check_handle.is_some() {
            warn!("Memory check task already started.");
            return;
        }
        info!(
            "Starting periodic memory check task with interval {:?}",
            interval
        );
        let task = tokio::spawn(Self::memory_check_task(
            interval,
            self.storage_type.clone(),
            self.redis_conn.clone(),
            Some(self.in_memory.clone()),
            self.max_redis_size,
            self.max_memory_size,
            self.current_redis_memory_usage_mb.clone(),
            self.current_memory_usage_mb.clone(),
        ));
        self._memory_check_handle = Some(task);
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
        let mut limiter = Limiter {
            storage_type: self
                .storage_config
                .storage_type
                .unwrap_or(StorageType::InMemory),
            redis_conn: self.storage_config.redis_conn,
            in_memory: Arc::new(DashMap::new()),
            max_memory_size: self.storage_config.max_memory_size,
            max_redis_size: self.storage_config.max_redis_size,

            acceptable_last_accesed_time_to_cache_redis_in_memory: self
                .storage_config
                .acceptable_last_accesed_time_to_cache_redis_in_memory
                .unwrap_or(Duration::from_secs(5)),

            current_redis_memory_usage_mb: Arc::new(AtomicU64::new(0)),
            current_memory_usage_mb: Arc::new(AtomicU64::new(0)),

            _memory_check_handle: None, // Placeholder
        };

        let check_interval = Duration::from_secs(5);
        limiter.start_memory_check_task(check_interval);

        limiter
    }
}

fn get_dashmap_approx_memory_size<K: Eq + std::hash::Hash, V>(map: &DashMap<K, V>) -> usize {
    map.len() * (std::mem::size_of::<K>() + std::mem::size_of::<V>())
        + map.capacity() * std::mem::size_of::<usize>() // Rough estimate
}
