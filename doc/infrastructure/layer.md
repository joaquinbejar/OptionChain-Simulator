# OptionChain-Simulator: Infrastructure Layer Architecture

This document outlines the infrastructure layer of the OptionChain-Simulator system, highlighting the components, their interactions, deployment options, and integration with the overall architecture.

## 1. Infrastructure Layer Components

The infrastructure layer provides essential technical capabilities to support the application and domain layers.

```mermaid
classDiagram
    class InfrastructureLayer {
        <<interface>>
    }

    class PersistenceAdapter {
        +saveSession(Session) void
        +loadSession(SessionId) Session
        +cleanupExpiredSessions() void
    }

    class LoggingAdapter {
        +logInfo(message, context) void
        +logError(error, context) void
        +logMetric(name, value, tags) void
    }

    class ConfigurationAdapter {
        +loadConfig() Configuration
        +saveConfig(Configuration) void
        +getParameter(key) Value
    }

    class MetricsCollector {
        +recordLatency(endpoint, time) void
        +incrementCounter(name) void
        +gaugeValue(name, value) void
    }

    class SchedulerAdapter {
        +scheduleTask(task, timing) TaskId
        +cancelTask(TaskId) void
        +listScheduledTasks() Task[]
    }

    class DataSourceAdapter {
        +fetchHistoricalData(asset, date) Data
        +storeSimulationResult(SimulationResult) void
        +queryTimeSeriesData(asset, period) TimeSeries
    }

    class CacheAdapter {
        +get(key) Value
        +set(key, value, ttl) void
        +delete(key) void
        +exists(key) boolean
    }

    InfrastructureLayer <|-- PersistenceAdapter
    InfrastructureLayer <|-- LoggingAdapter
    InfrastructureLayer <|-- ConfigurationAdapter
    InfrastructureLayer <|-- MetricsCollector
    InfrastructureLayer <|-- SchedulerAdapter
    InfrastructureLayer <|-- DataSourceAdapter
    InfrastructureLayer <|-- CacheAdapter
```

## 2. Infrastructure Deployment Architecture

This diagram shows how the infrastructure components are deployed and integrated with the other layers of the system.

```mermaid
flowchart TB
    subgraph "OptionChain-Simulator System"
        API[API Gateway]
        App[Application Services]
        Domain[Domain Layer]

        subgraph "Infrastructure Layer"
            Redis[(Redis\nSession Store & Cache)]
            ClickHouse[(ClickHouse\nHistorical Data)]
            MongoDB[(MongoDB\nConfig)]
            Prometheus[Prometheus\nMetrics]
            Jaeger[Jaeger\nTracing]
            TaskQueue[Task Queue]
        end

        API --> App
        App --> Domain
        App --> Redis
        App --> ClickHouse
        App --> MongoDB
        App --> Prometheus
        App --> Jaeger
        App --> TaskQueue
    end

    Client[Client Applications] --> API
    DataSources[External Data Sources] --> ClickHouse
```

## 3. Session Management with Infrastructure Components

This sequence diagram illustrates how different infrastructure components interact during session creation and usage.

```mermaid
sequenceDiagram
    participant Client
    participant APILayer as API Layer
    participant AppLayer as Application Layer
    participant DomainLayer as Domain Layer
    participant Redis as Session Store & Cache (Redis)
    participant ClickHouse as Historical Data (ClickHouse)
    participant MongoDB as Config (MongoDB)
    participant Metrics as Metrics Collector
    participant Logs as Logging Service

    Client->>APILayer: Create Session (POST)
    APILayer->>AppLayer: Forward Request
    AppLayer->>MongoDB: Load Configuration
    MongoDB-->>AppLayer: Return Configuration
    AppLayer->>DomainLayer: Create Domain Objects
    DomainLayer-->>AppLayer: Return Domain Objects
    AppLayer->>Redis: Save Session
    Redis-->>AppLayer: Confirm Save
    AppLayer->>Metrics: Record 'SessionCreated'
    AppLayer->>Logs: Log Session Creation
    AppLayer-->>APILayer: Return Session Details
    APILayer-->>Client: Session Created Response

    Note over Client,Logs: Session Usage Flow

    Client->>APILayer: Get Next Step (GET)
    APILayer->>Redis: Retrieve Session
    Redis-->>APILayer: Return Session
    APILayer->>AppLayer: Process Next Step
    AppLayer->>DomainLayer: Calculate Next Values
    DomainLayer-->>AppLayer: Return Updated Chain
    AppLayer->>Redis: Update Session State
    AppLayer->>Redis: Cache Chain Results
    AppLayer->>Metrics: Record 'StepCalculation' Latency
    AppLayer-->>APILayer: Return Updated Chain
    APILayer-->>Client: Chain Data Response
```

## 4. Data Persistence Infrastructure Design

The system implements specific storage solutions for different types of data.

```mermaid
flowchart LR
    subgraph "Session Management"
        direction TB
        RedisSessionStore[(Redis)]
        SessionManager[Session Manager]
        SessionManager --> RedisSessionStore
    end

    subgraph "Historical Data Storage"
        direction TB
        ClickHouseHistorical[(ClickHouse\nOHLCV Tables)]
        HistoricalDataService[Historical Data Service]
        HistoricalDataService --> ClickHouseHistorical
    end

    subgraph "Configuration Storage"
        direction TB
        MongoConfig[(MongoDB\nConfig Collections)]
        ConfigService[Config Service]
        ConfigService --> MongoConfig
    end

    subgraph "Caching Layer"
        direction TB
        RedisCache[(Redis Cache)]
        CacheService[Cache Service]
        CacheService --> RedisCache
    end

    SessionManager -.-> CacheService
    HistoricalDataService -.-> CacheService
    ConfigService -.-> CacheService
```

## 5. Infrastructure Monitoring & Observability

This diagram shows how monitoring and observability are implemented across the system.

```mermaid
flowchart TB
    subgraph "OptionChain-Simulator"
        API[API Layer]
        App[Application Layer]
        Infra[Infrastructure Layer]
    end

    subgraph "Observability Infrastructure"
        direction LR
        Prometheus[Prometheus\nMetrics Storage]
        Grafana[Grafana\nDashboards]
        Jaeger[Jaeger\nDistributed Tracing]
        Loki[Loki\nLog Aggregation]
        AlertManager[Alert Manager]
    end

    API -- "Metrics\nExporter" --> Prometheus
    App -- "Metrics\nExporter" --> Prometheus
    Infra -- "Metrics\nExporter" --> Prometheus

    API -- "Trace\nContext" --> Jaeger
    App -- "Trace\nContext" --> Jaeger
    Infra -- "Trace\nContext" --> Jaeger

    API -- "Structured\nLogs" --> Loki
    App -- "Structured\nLogs" --> Loki
    Infra -- "Structured\nLogs" --> Loki

    Prometheus --> Grafana
    Jaeger --> Grafana
    Loki --> Grafana

    Prometheus --> AlertManager
```

## 6. Detailed Infrastructure Components

This diagram shows the interfaces and concrete implementations for key infrastructure components aligned with our technology choices.

```mermaid
classDiagram
    class SessionStore {
        <<interface>>
        +get(SessionId) Session
        +save(Session) void
        +delete(SessionId) void
        +listActive() SessionId[]
    }

    class RedisSessionStore {
        -redisClient RedisClient
        +get(SessionId) Session
        +save(Session) void
        +delete(SessionId) void
        +listActive() SessionId[]
    }

    class CacheService {
        <<interface>>
        +get(key, type) Value
        +set(key, value, ttl) void
        +delete(key) void
        +exists(key) boolean
    }

    class RedisCacheService {
        -redisClient RedisClient
        +get(key, type) Value
        +set(key, value, ttl) void
        +delete(key) void
        +exists(key) boolean
    }

    class HistoricalDataRepository {
        <<interface>>
        +getHistoricalChain(asset, date) OptionChain
        +saveHistoricalChain(OptionChain) void
        +listAvailableAssets() Asset[]
        +getDateRangeForAsset(asset) DateRange
    }

    class ClickHouseHistoricalRepository {
        -clickhouseClient ClickHouseClient
        +getHistoricalChain(asset, date) OptionChain
        +saveHistoricalChain(OptionChain) void
        +listAvailableAssets() Asset[]
        +getDateRangeForAsset(asset) DateRange
    }

    class ConfigRepository {
        <<interface>>
        +getConfig(name) Configuration
        +saveConfig(Configuration) void
        +listConfigurations() ConfigMetadata[]
        +getConfigHistory(name) ConfigVersion[]
    }

    class MongoConfigRepository {
        -mongoClient MongoClient
        -db Database
        -collection Collection
        +getConfig(name) Configuration
        +saveConfig(Configuration) void
        +listConfigurations() ConfigMetadata[]
        +getConfigHistory(name) ConfigVersion[]
    }

    class MetricsService {
        <<interface>>
        +recordRequestLatency(endpoint, ms) void
        +recordSimulationPerformance(steps, ms) void
        +incrementSessionCount() void
        +decrementSessionCount() void
        +recordMemoryUsage(bytes) void
    }

    class PrometheusMetricsService {
        -latencyHistogram Histogram
        -sessionGauge Gauge
        -simulationCounter Counter
        +recordRequestLatency(endpoint, ms) void
        +recordSimulationPerformance(steps, ms) void
        +incrementSessionCount() void
        +decrementSessionCount() void
        +recordMemoryUsage(bytes) void
    }

    SessionStore <|-- RedisSessionStore
    CacheService <|-- RedisCacheService
    HistoricalDataRepository <|-- ClickHouseHistoricalRepository
    ConfigRepository <|-- MongoConfigRepository
    MetricsService <|-- PrometheusMetricsService
```

## 7. Infrastructure Implementation Recommendations

### Storage Technologies

1. **Session Storage & Cache**
   - **Redis**: Fast, in-memory data store for both session management and caching
   - Configuration:
      - Enable persistence with RDB snapshots and AOF logs
      - Use Redis Cluster for high availability in production
      - Configure appropriate eviction policies for cache data

2. **Historical Data**
   - **ClickHouse**: Column-oriented database for time-series OHLCV data
   - Configuration:
      - Use `MergeTree` engine with `ORDER BY (symbol, timestamp)`
      - Partition by date for faster queries
      - Enable compression and deduplication as needed

3. **Configuration Data**
   - **MongoDB**: Document-oriented store for config JSON
   - Configuration:
      - Create indexed collections per config type
      - Use versioned documents for config history

## 8. Scaling Considerations

- ClickHouse clustering with `ReplicatedMergeTree`, distributed tables
- Use materialized views for aggregated rollups

## 9. Infrastructure Security

- ClickHouse user authentication and TLS configuration
- Proper access roles and IP whitelisting

## 10. Disaster Recovery

- Use `clickhouse-backup` for snapshots
- Replicated tables for HA
- Scheduled backup to cloud storage or offsite location

