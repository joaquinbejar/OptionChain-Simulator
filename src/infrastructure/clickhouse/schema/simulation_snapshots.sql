-- One row per materialised v2 simulation step: the snapshot's metadata and its
-- completion marker (issue #56).
--
-- ENGINE — ReplacingMergeTree(inserted_at_ms)
--   A snapshot is immutable and deterministic: rebuilding step N of a given
--   (simulation, version) always produces the same values. So a retry, a
--   replay, or a backfill must be able to write the same coordinate again
--   without ever exposing two rows for it. ReplacingMergeTree collapses rows
--   sharing the sorting key and keeps the one with the greatest version column;
--   `inserted_at_ms` (unix milliseconds) makes that "the most recent write".
--   Two writes inside one millisecond tie, which is harmless — the rows are
--   identical by construction. Because merges are asynchronous, every read
--   issued by the service uses FINAL (or an explicit deduplicating aggregate),
--   so idempotence holds immediately, not eventually.
--
-- ORDER BY — (simulation_id, simulation_generation, step)
--   The identity of a snapshot, and therefore the deduplication key. It is also
--   the access path of both query patterns: reconstructing one step, and
--   scanning a step range of one simulation.
--
-- PARTITION BY — toYYYYMM(simulated_at)
--   Deliberately a function of the row's OWN content rather than of when it was
--   written. ReplacingMergeTree only deduplicates within a partition, so a
--   partition key derived from the ingestion time would let a backfill written
--   a month after the original land in a different partition and survive as a
--   duplicate forever. `simulated_at` is deterministic for a coordinate, so a
--   retry always lands in the partition its original did. It also keeps one
--   snapshot's rows in exactly one partition (every row shares the instant), so
--   an insert creates one part.
--
-- CODECs
--   The columns are overwhelmingly low-entropy within a part: a block holds one
--   simulation, one version, consecutive steps, one symbol. Delta/DoubleDelta
--   turns the counters and the timestamps into runs of near-zeros, and ZSTD(1)
--   compresses the residue and the repeated identifiers to almost nothing.
--
-- TTL — retention measured from INGESTION, not from simulated time
--   A simulation's clock may sit years in the future, so a TTL on
--   `simulated_at` would either never fire or fire immediately depending on the
--   scenario. `inserted_at_ms` is real time. Row-level rather than
--   partition-level for the same reason the partition key is what it is.
--   Changing the retention of an EXISTING table needs an explicit
--   `ALTER TABLE ... MODIFY TTL`: `CREATE TABLE IF NOT EXISTS` will not alter
--   a table that is already there.
--
-- The ids are String, not the native UUID type, DELIBERATELY
--   Native UUID would halve the sorting key (16 bytes against 36) and would
--   read back fine, but the INSERT path cannot use it: the driver validates
--   every RowBinary column against the Rust field type, and mapping a UUID
--   column needs `clickhouse/serde::uuid`, which lives behind a crate feature
--   this workspace does not enable. Turning it on is a `[workspace.dependencies]`
--   change, so it is a deliberate follow-up rather than something to smuggle in
--   here. The cost is bounded in practice: the value is constant across every
--   row of a part and ZSTD collapses it, and it is only the leading key column,
--   never a scanned one.
--
-- `simulation_generation` is NOT the session's `version`
--   A v2 session's `version` is its optimistic-concurrency revision and changes
--   on every advance; feeding it in here would scatter one tape across hundreds
--   of coordinates and defeat deduplication. The generation changes only when
--   the crate would produce different values for the same step — see
--   CURRENT_SNAPSHOT_GENERATION in
--   `src/infrastructure/clickhouse/snapshots/record.rs`.
CREATE TABLE IF NOT EXISTS simulation_snapshots
(
    simulation_id           String CODEC(ZSTD(1)),
    simulation_generation   UInt64 CODEC(DoubleDelta, ZSTD(1)),
    step                    UInt64 CODEC(DoubleDelta, ZSTD(1)),
    snapshot_id             String CODEC(ZSTD(1)),
    simulated_at            DateTime64(9, 'UTC') CODEC(DoubleDelta, ZSTD(1)),
    symbol                  LowCardinality(String) CODEC(ZSTD(1)),
    underlying_price        Decimal(38, 28) CODEC(ZSTD(1)),
    base_volatility         Decimal(38, 28) CODEC(ZSTD(1)),
    quote_count             UInt64 CODEC(DoubleDelta, ZSTD(1)),
    expiration_count        UInt64 CODEC(DoubleDelta, ZSTD(1)),
    complete                Bool CODEC(ZSTD(1)),
    inserted_at_ms          UInt64 CODEC(DoubleDelta, ZSTD(1))
)
ENGINE = ReplacingMergeTree(inserted_at_ms)
PARTITION BY toYYYYMM(simulated_at)
ORDER BY (simulation_id, simulation_generation, step)
TTL toDateTime(intDiv(inserted_at_ms, 1000)) + INTERVAL {{RETENTION_DAYS}} DAY DELETE
