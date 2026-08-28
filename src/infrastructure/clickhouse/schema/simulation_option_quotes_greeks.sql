-- Adds the per-style Greek columns of issue #74 to an EXISTING
-- `simulation_option_quotes`.
--
-- WHY THIS FILE EXISTS
--   `CREATE TABLE IF NOT EXISTS` does not alter a table that already exists, so
--   a deployment that predates issue #74 would boot green and then fail every
--   insert with `database schema has no column named gamma_call`, one WARN per
--   step, while its warehouse silently stopped filling. That is precisely the
--   failure the boot-time schema check exists to prevent, so the migration is
--   RUN at startup rather than documented for an operator to hand-expand.
--
--   `ADD COLUMN IF NOT EXISTS` is idempotent and metadata-only on a MergeTree:
--   it does not rewrite existing parts, and existing rows read the new columns
--   as NULL — which is exactly how a pre-#74 row is meant to read.
--
-- ORDER
--   Run AFTER the `CREATE TABLE IF NOT EXISTS` of the same table. On a fresh
--   deployment every column already exists and this is a no-op; on an existing
--   one it is the upgrade. The physical column order therefore differs between
--   a migrated table and a fresh one, which is harmless: every INSERT and every
--   SELECT in this crate names its columns.
--
-- The column list must stay in step with `simulation_option_quotes.sql`; a test
-- asserts that it does.
ALTER TABLE simulation_option_quotes
    ADD COLUMN IF NOT EXISTS gamma_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS gamma_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS theta_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS theta_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vega_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vega_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS rho_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS rho_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS rho_d_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS rho_d_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS alpha_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS alpha_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vanna_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vanna_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vomma_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS vomma_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS veta_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS veta_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS charm_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS charm_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS color_call Nullable(Decimal(38, 28)) CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS color_put Nullable(Decimal(38, 28)) CODEC(ZSTD(1))
