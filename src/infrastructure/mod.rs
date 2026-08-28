mod clickhouse;
mod config;
mod mongodb;
mod redis;
mod repositories;
mod telemetry;

pub use clickhouse::ClickHouseClient;
pub use clickhouse::interface::HistoricalDataRepository;
pub use clickhouse::model::{OHLCVData, PriceType};
pub use clickhouse::snapshots::interface::{ContractSeriesQuery, SimulationSnapshotRepository};
pub use clickhouse::snapshots::record::{
    CURRENT_SNAPSHOT_GENERATION, ContractQuote, ContractSide, ExpirationRecord, QuoteRow,
    SnapshotRecord, snapshot_id,
};
pub(crate) use clickhouse::{calculate_required_duration, select_random_date, validate_symbol};
pub use config::clickhouse::ClickHouseConfig;
pub use config::logging::{
    DEFAULT_LOG_LEVEL, LOG_LEVEL_VAR, LogLevel, ResolvedLogLevel, resolve_log_level,
    resolve_log_level_from_env,
};
pub use config::redis::RedisConfig;
pub use config::server::{
    BIND_ADDRESS_VAR, DEFAULT_BIND_ADDRESS, DEFAULT_PORT, ListenOn, PORT_VAR, ServerConfig,
};
pub use config::simulation_v2::{
    DEFAULT_CLEANUP_INTERVAL_SECS, DEFAULT_MAX_CACHED_SNAPSHOT_CONTRACTS,
    DEFAULT_MAX_CACHED_SNAPSHOTS, DEFAULT_MAX_CACHED_TAPES, DEFAULT_MAX_EXPORT_ROWS,
    DEFAULT_MAX_SNAPSHOT_CONTRACTS, DEFAULT_RETENTION_SECS, SimulationV2Config,
    max_snapshot_contracts,
};
pub use config::snapshot::{
    DEFAULT_SNAPSHOT_BATCH_ROWS, DEFAULT_SNAPSHOT_INSERT_TIMEOUT_SECS,
    DEFAULT_SNAPSHOT_MAX_READ_ROWS, DEFAULT_SNAPSHOT_PERSISTENCE_ENABLED,
    DEFAULT_SNAPSHOT_RETENTION_DAYS, SnapshotPersistenceConfig,
};
pub use redis::RedisClient;
pub use repositories::historical_repo::ClickHouseHistoricalRepository;
pub use repositories::mongo_repo::{MongoDBRepository, init_mongodb};
pub use repositories::snapshot_repo::ClickHouseSnapshotRepository;
pub use telemetry::collector::MetricsCollector;
pub use telemetry::middleware::MetricsMiddleware;
