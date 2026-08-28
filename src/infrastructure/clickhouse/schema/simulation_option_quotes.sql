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
-- Greek columns — the full set, per option style (issue #74)
--   A quote carries `optionstratlib`'s twelve-value `GreeksSnapshot` for each
--   style: delta, gamma, theta, vega, rho, rho_d, alpha, vanna, vomma, veta,
--   charm, color. Twenty-four columns, not two JSON blobs per side: the columns
--   stay queryable, and a column store can read one greek without paying for
--   the other twenty-three.
--
--   THEY ARE NOT CHEAP. Measured on a real tape — 9 115 rows, 40 steps, three
--   expirations, 101 strikes, priced by upstream rather than repeated from a
--   fixture — the table grows from 121 to 363 compressed bytes per row, a
--   factor of THREE, or about 11 bytes per greek. The rest of this file's
--   low-entropy reasoning does not extend to them: a `Decimal(38, 28)` greek is
--   a 16-byte integer whose low-order bytes carry real, uncorrelated precision,
--   so ZSTD recovers only about a third of it. Budget retention accordingly —
--   `RETENTION_DAYS` now buys a third of the history it used to for the same
--   disk.
--
--   The alternatives were priced against this server rather than assumed, so
--   the next reader does not have to re-ask. `Delta`, `DoubleDelta`, `Gorilla`
--   and `T64` are all REJECTED BY CLICKHOUSE for `Decimal(38, 28)` — they
--   require a 1, 2, 4 or 8-byte type. Raising the ZSTD level does nothing
--   (measured 11.98 B/value at ZSTD(1), 12.13 at ZSTD(3), 12.04 at ZSTD(9)):
--   there is no structure left to find. The only real lever is narrowing the
--   type — `Decimal64(12) CODEC(T64, ZSTD(1))` measures 4.63 B/value and would
--   put the row near 223 bytes instead of 363 — and it is not taken, because it
--   trades away the exact round trip that the whole Decimal(38, 28) choice
--   above exists to guarantee.
--
--   `gamma` was SPLIT into `gamma_call` / `gamma_put`. For a European option
--   the two are always equal — upstream computes both from one shared kernel —
--   so this is not redundancy but future-proofing: upstream's `gamma()` falls
--   through to a numerical estimate for non-European types, which IS
--   style-dependent, while the mirror is only ever built as a Call.
--
--   The original shared `gamma` column is KEPT and still written: it is
--   upstream's convenience mirror, computed independently of the snapshots and
--   still defined at expiry and at zero volatility where the full set is not.
--   Rows written before this change carry it and nothing else, so dropping it
--   would strand them.
--
--   `delta_call` / `delta_put` were already per-style, so they serve as BOTH
--   the mirror and the snapshot's delta rather than being stored twice. That
--   rests on the two agreeing, which upstream does not promise in writing, so
--   `test_the_delta_mirror_equals_the_snapshot_delta_at_every_strike` pins it
--   and will fail loudly on the release that changes it.
--
--   Every greek column is `Nullable`. A strike whose option cannot be built has
--   no snapshot at all, and NULL is that absence — never a zero. A row written
--   before this change reads back with the new columns NULL and therefore with
--   no reconstructed snapshot, which is the honest answer.
--
-- MIGRATION — `CREATE TABLE IF NOT EXISTS` does NOT alter an existing table,
--   so the greek columns are added by `simulation_option_quotes_greeks.sql`,
--   which `ensure_schema` RUNS at startup right after this file. Nothing is
--   left for an operator to expand by hand: an elided migration is one that
--   gets typed wrong, and the failure mode is a warehouse that boots green and
--   then silently stops filling.
--
--   The physical column order therefore differs between a migrated table and a
--   freshly created one. That is harmless because every INSERT and every SELECT
--   in this crate names its columns.
--
--   ROLLBACK is safe for the same reason. An older binary's INSERT names the
--   columns it knows and the greek columns take their NULL default; its SELECT
--   names a subset and ignores the rest. Downgrading loses the greeks of the
--   steps written while it is running, which then read back as no snapshot —
--   the same as any pre-#74 row — and loses nothing else.
--
--   TTL is the deliberate exception and is still not migrated — see the note
--   below.
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
    gamma_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    gamma_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    theta_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    theta_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vega_call               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vega_put                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    rho_call                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    rho_put                 Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    rho_d_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    rho_d_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    alpha_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    alpha_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vanna_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vanna_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vomma_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    vomma_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    veta_call               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    veta_put                Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    charm_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    charm_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    color_call              Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    color_put               Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    inserted_at_ms          UInt64 CODEC(DoubleDelta, ZSTD(1)),
    INDEX idx_contract (expires_at, strike) TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(inserted_at_ms)
PARTITION BY toYYYYMM(simulated_at)
PRIMARY KEY (simulation_id, simulation_generation, step)
ORDER BY (simulation_id, simulation_generation, step, expires_at, strike)
TTL toDateTime(intDiv(inserted_at_ms, 1000)) + INTERVAL {{RETENTION_DAYS}} DAY DELETE
