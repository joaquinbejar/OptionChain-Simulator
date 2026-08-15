-- One row per (v2 simulation step, physical expiration, strike) — both sides of
-- the strike, since a call and a put share the strike, the implied volatility
-- and the gamma (issue #56).
--
-- ENGINE — ReplacingMergeTree(inserted_at_ms)
--   Same reasoning as `simulation_snapshots`: a retried or replayed snapshot
--   rewrites the same coordinates, and the engine collapses them to the most
--   recent write. Reads use FINAL (or `uniqExact` over the identity columns
--   when only a count is needed) so idempotence holds immediately rather than
--   after the next background merge.
--
-- ORDER BY — (simulation_id, simulation_generation, step, expires_at, strike)
--   The row's identity, and the deduplication key. The prefix is chosen for the
--   dominant query pattern, whole-snapshot reconstruction: one step of one
--   simulation is a contiguous run, read as a single range.
--
--   PRIMARY KEY stops at `step`, so the sparse primary index stays small: the
--   two trailing columns still participate in deduplication and in the sort
--   order, they just do not enlarge the in-memory index.
--
-- The second query pattern — one contract across time — filters
-- (expires_at, strike) while ranging over steps, which the ORDER BY prefix
-- cannot serve directly. Two things make it cheap anyway. The
-- (simulation_id, simulation_generation) prefix already restricts the scan to one
-- simulation, and within each step's block the rows are sorted by expiry and
-- then strike, so the granules of a block cover contiguous (expiry, strike)
-- intervals — exactly what a minmax skip index prunes on. Hence idx_contract
-- with GRANULARITY 1, evaluated per granule.
--
-- PARTITION BY — toYYYYMM(simulated_at)
--   A function of the row's own content, so a backfill written months later
--   lands in the partition its original did and can be deduplicated against it.
--   Also keeps one snapshot's rows in one partition, so one snapshot inserts as
--   one already-sorted part.
--
-- Monetary and Greek columns — Decimal(38, 28)
--   The tape must reconstruct to exactly what was generated; Float64 cannot
--   promise that for a `rust_decimal` premium. Scale 28 is `rust_decimal`'s
--   maximum, so nothing written here is ever rounded. See
--   `src/infrastructure/clickhouse/snapshots/model.rs`.
--
-- Snapshot-level columns (snapshot_id, simulated_at, symbol) are repeated on
-- every row on purpose: they are constant within a part and compress to
-- nothing, and they let a contract history be served without joining.
--
-- TTL — retention measured from INGESTION. See `simulation_snapshots.sql`.
--
-- The ids are String rather than the native UUID type, and
-- `simulation_generation` is not the session's `version`. Both choices are
-- explained in full in `simulation_snapshots.sql`; the two tables must agree on
-- them, since they are joined on exactly these columns.
CREATE TABLE IF NOT EXISTS simulation_option_quotes
(
    simulation_id           String CODEC(ZSTD(1)),
    simulation_generation   UInt64 CODEC(DoubleDelta, ZSTD(1)),
    step                    UInt64 CODEC(DoubleDelta, ZSTD(1)),
    expires_at              DateTime64(9, 'UTC') CODEC(DoubleDelta, ZSTD(1)),
    strike                  Decimal(38, 28) CODEC(ZSTD(1)),
    snapshot_id             String CODEC(ZSTD(1)),
    simulated_at            DateTime64(9, 'UTC') CODEC(DoubleDelta, ZSTD(1)),
    symbol                  LowCardinality(String) CODEC(ZSTD(1)),
    days_to_expiration      Decimal(38, 28) CODEC(ZSTD(1)),
    labels                  Array(LowCardinality(String)) CODEC(ZSTD(1)),
    implied_volatility      Decimal(38, 28) CODEC(ZSTD(1)),
    call_bid                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    call_ask                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    call_mid                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    put_bid                 Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    put_ask                 Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    put_mid                 Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    delta_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    delta_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    gamma                   Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    inserted_at_ms          UInt64 CODEC(DoubleDelta, ZSTD(1)),
    INDEX idx_contract (expires_at, strike) TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(inserted_at_ms)
PARTITION BY toYYYYMM(simulated_at)
PRIMARY KEY (simulation_id, simulation_generation, step)
ORDER BY (simulation_id, simulation_generation, step, expires_at, strike)
TTL toDateTime(intDiv(inserted_at_ms, 1000)) + INTERVAL {{RETENTION_DAYS}} DAY DELETE
