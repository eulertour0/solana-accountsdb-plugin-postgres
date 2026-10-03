/**
 * Migration for the agave v4.3 build of the plugin (branch agave-v4.3).
 *
 * Run this ONCE against an existing database created from an older
 * create_schema.sql, BEFORE loading the new plugin:
 *
 *   psql -h 127.0.0.1 -U solana -d solana -v ON_ERROR_STOP=1 -f scripts/migrate_agave_v4_3.sql
 *
 * Every statement is additive and idempotent (safe to re-run), and none of
 * them rewrites a table:
 *   - ALTER TYPE ... ADD VALUE only appends an enum label (catalog change).
 *   - Widening a VARCHAR(n) column to a larger n is a catalog-only change in
 *     PostgreSQL >= 9.2; no table rewrite and no index rebuild.
 * The transaction table (and its composite-typed columns) is not touched.
 *
 * ALTER TYPE ... ADD VALUE cannot be used in the same transaction that adds
 * it, so do not wrap this file in BEGIN/COMMIT together with any writes.
 */

-- TransactionError variants added since the original schema
-- (solana-transaction-error 3.x). Without them, a transaction failing with
-- one of these errors could not be stored.
ALTER TYPE "TransactionErrorCode" ADD VALUE IF NOT EXISTS 'ProgramCacheHitMaxLimit';
ALTER TYPE "TransactionErrorCode" ADD VALUE IF NOT EXISTS 'CommitCancelled';
-- Reserved fallback label for errors the plugin cannot classify.
ALTER TYPE "TransactionErrorCode" ADD VALUE IF NOT EXISTS 'Unknown';

-- RewardType variants added since the original schema (solana-reward-info 7.x).
ALTER TYPE "RewardType" ADD VALUE IF NOT EXISTS 'DeactivatedStake';
ALTER TYPE "RewardType" ADD VALUE IF NOT EXISTS 'VATDebit';

-- Slot status names are written in full (e.g. 'first_shred_received', 20
-- characters) instead of being truncated to 16 characters. 'rooted' is
-- unchanged. Catalog-only change, holds only a brief ACCESS EXCLUSIVE lock.
ALTER TABLE slot ALTER COLUMN status TYPE VARCHAR(32);

-- Optional cleanup of rows written by the old plugin with a truncated status.
-- This UPDATEs rows (no table rewrite) and is not required by the plugin;
-- uncomment to run it.
-- UPDATE slot SET status = 'first_shred_received' WHERE status = 'first_shred_rece';
