The `solana-geyser-plugin-postgres` crate implements a plugin storing
account data to a PostgreSQL database to illustrate how a plugin can be
developed to work with Solana validators using the [Plugin Framework](https://docs.solana.com/developing/plugins/geyser-plugins).

### Building for agave v4.4.1

A Geyser plugin is loaded with `dlopen` and shares Rust trait objects and
`repr(Rust)` types (`VersionedTransaction`, `LoadedAddresses`,
`TransactionError`, `log::Record`, ...) with the validator. It is only safe to
load when it is built with the same rustc and from the same crate versions as
the validator. A plugin built for agave 4.2 crashes a 4.3 validator at startup,
and a 4.3 plugin cannot be loaded by a 4.4 validator either (the transaction
and block notification versions it handles no longer exist).

This branch (`agave-v4.4`) targets **agave v4.4.1** (tag `v4.4.1`, commit
`5bb304027f`):

* Since agave v4.4 the interface is the crates.io crate
  `agave-geyser-plugin-interface` **5.0.0** (agave's workspace pins it with
  `=`). It no longer exposes `solana-transaction-status` types: transactions
  arrive as `ReplicaTransactionInfoVersions::V0_0_4` and blocks as
  `ReplicaBlockInfoVersions::V0_0_5`, with the status meta and rewards as the
  borrowed mirror types of its `transaction_status_meta` module. The plugin
  maps those mirrors straight into its row types, so the plugin library has no
  agave path dependency.
* `pg_backfill` (feature `backfill`) still needs agave's
  `solana-transaction-status` / `solana-transaction-context` for the RPC
  types. They are path dependencies on the agave source, expected as a sibling
  directory named `agave-v4.4.1` next to this repository (`../agave-v4.4.1`).
  If the validator source lives elsewhere, create a symlink at that path
  pointing to it (or edit the two `path =` entries in `Cargo.toml`). The default
  (plugin) build does not read it, but `scripts/check_agave_abi_deps.sh` does.
* `rust-toolchain.toml` pins rustc **1.98.1** (agave v4.4.1's toolchain).
* `Cargo.lock` was seeded from agave v4.4.1's `Cargo.lock`, and the registry
  crates whose types cross the plugin boundary are pinned with `=` to the same
  versions (`solana-message` 5.0.0, `solana-transaction` 5.0.0,
  `solana-transaction-error` 4.0.0, `solana-clock` 4.0.0, `log` 0.4.34, ...).
  `scripts/check_agave_abi_deps.sh` verifies that every crate shared with
  `agave-validator` resolves to the validator's version.

Build steps on mainnet-la (validator built from the same v4.4.1 tree):

```
rustup toolchain install 1.98.1            # if not installed yet
cd ~                                       # parent of this repository
git clone --branch v4.4.1 --depth 1 https://github.com/anza-xyz/agave.git agave-v4.4.1
#   or: ln -s /path/to/the/validator/agave/source agave-v4.4.1
cd solana-accountsdb-plugin-postgres
git checkout agave-v4.4
scripts/check_agave_abi_deps.sh            # must print "OK: ..."
cargo build --release --locked
cargo run --release --locked --example validator_abi_smoke -- \
    $PWD/target/release/libsolana_geyser_plugin_postgres.so
```

The ABI smoke tool `dlopen`s the built library, installs a logger through
`setup_logger`, and drives every callback agave v4.4.1 uses
(`update_account_from_snapshot`, `update_account_for_bank` with
`ReplicaAccountInfoV3`, `notify_transaction_for_bank` with
`ReplicaTransactionInfoV4`, `notify_entry_for_bank`, `notify_block_footer`,
`update_slot_status`, `update_bank_status`, `notify_block_metadata_for_bank`
with `ReplicaBlockInfoV5`, `notify_end_of_startup`, `on_unload`) with every
optional meta field populated. It compares the complete rows the loaded library
queued (taken from its trace log) with the rows the crate builds in-process
from the same values. It needs no PostgreSQL server and no validator build.

Upgrading from the agave v4.3 build needs no database change: rows are written
exactly as before (same columns, same values). `TransactionError` became
`#[non_exhaustive]` and gained `BailOut` (a leader-side discard that is never
committed, so it should not reach the plugin); it and any later variant are
stored with the existing `Unknown` label and the error's `Debug` text in
`error_detail`.

### Upgrading an existing database (agave v4.3 build)

Run the migration once, **before** loading the new plugin (stop the validator
or unload the old plugin first so that the brief `ALTER TABLE slot` lock is not
contended):

```
psql -h 127.0.0.1 -U solana -d solana -v ON_ERROR_STOP=1 -f scripts/migrate_agave_v4_3.sql
```

It is additive, idempotent and does not rewrite any table: it adds the
`TransactionErrorCode` labels `ProgramCacheHitMaxLimit`, `CommitCancelled`,
`Unknown`, the `RewardType` labels `DeactivatedStake`, `VATDebit`, and widens
`slot.status` from `VARCHAR(16)` to `VARCHAR(32)`. New databases created with
`scripts/create_schema.sql` already contain these changes.

`on_load` checks for these labels and the column width and refuses to load
(the validator does not start) with a message pointing at the migration if they
are missing. The check is skipped when the database is unreachable at load time.

Behavior of this build:

* Slot statuses are written in full (`first_shred_received`, `created_bank`,
  ...). A row whose status is `rooted` is never overwritten by a later, lower
  status from another worker thread.
* `transaction.message_type` is 0 (legacy, `legacy_message`), 1 (v0,
  `v0_loaded_message`) or 2 (v1 messages, stored in `legacy_message`; v1 has no
  address lookup tables). `(v0_loaded_message).loaded_addresses` now holds the
  ALT-resolved addresses from the transaction status meta.
* `panic_on_db_errors: true` still aborts the validator on connection or other
  database failures, but a row the database rejects (SQLSTATE class 22/23, e.g.
  an enum label missing because the migration was not run) is logged and
  skipped. Set `"panic_on_row_errors": true` to abort on those too.
* Invalid keys in `accounts_selector` / `transaction_selector` make `on_load`
  fail with an error instead of panicking; duplicate mentions are ignored.
* Plugin logs go to the validator's log through `setup_logger`.
* `"store_log_messages": false` writes `(meta).log_messages` as NULL instead of
  the transaction's program logs (default `true`). Logs are about a quarter of
  the on-disk size of the `transaction` table.

Example configuration (transactions only, accounts disabled):

```
{
    "libpath": "/home/solana/solana-accountsdb-plugin-postgres/target/release/libsolana_geyser_plugin_postgres.so",
    "host": "127.0.0.1",
    "user": "solana",
    "port": 5432,
    "threads": 20,
    "batch_size": 20,
    "panic_on_db_errors": true,
    "panic_on_row_errors": false,
    "store_log_messages": false,
    "accounts_selector": {},
    "transaction_selector": {
        "mentions": ["pubkey-1", "pubkey-2"]
    }
}
```

### Backfilling a gap from RPC (`pg_backfill`)

`src/bin/pg_backfill.rs` fills `transaction` rows (plus `slot` and optionally
`block` rows) for a slot range the plugin did not see, from a JSON-RPC node
with transaction history (`https://api.mainnet-beta.solana.com` by default).
Rows are built by the plugin's own code: the `getTransaction` reply (base64) is
converted back into the `VersionedTransaction` + `TransactionStatusMeta` the
validator builds (`src/rpc_convert.rs`, the inverse of agave's RPC encoder),
borrowed into the interface's mirror types the way agave's geyser plugin
manager does before notifying a plugin (`with_mirror_meta`), filtered with the plugin's `TransactionSelector` (static and
ALT-loaded keys), turned into a row by `build_db_transaction` and written with
the plugin's prepared upserts (`ON CONFLICT (slot, signature) DO UPDATE`, slot
rows as `rooted` through the `WHERE status <> 'rooted'` guard).

Two phases, both resumable:

* `collect` pages `getSignaturesForAddress` (1000 per page, `finalized`) for
  every mention and appends the signatures inside the range, deduplicated across
  addresses, to `<work-dir>/queue.tsv`. The paging cursor of every address is
  saved in `<work-dir>/cursors.json` after each page.
* `fetch` streams `queue.tsv`, fetches each signature with `getTransaction` and
  writes the row. Every finished signature is appended to `<work-dir>/done.tsv`
  (`ok`, `not_selected`, `unsupported_version`, `out_of_range`) after its rows
  are written; signatures that failed (RPC gave up, row rejected by the
  database) go to `<work-dir>/failed.tsv` and are retried by the next run.

Memory stays small at any volume: signatures are deduplicated by a 16-byte key
(about 8 MB per million) and the work files are streamed. Killing the process
at any point is safe; run the same command again to continue. A re-run never
duplicates rows (all writes are upserts), so deleting `done.tsv` just re-does
the work. `<work-dir>/lock` prevents two runs on the same work directory, and
the work directory remembers its slot range and refuses another one.

Fidelity: for the 100 transactions the live plugin (agave v4.3.0) wrote on
mainnet-la after the gap, the rows built from `getTransaction` are identical to
the stored rows in every column except `write_version` and `updated_on`
(compared through `row_to_json`; one of them is a unit-test fixture, together
with a v0 row with zero token balances from the old run). What cannot be taken
from RPC as the plugin had it:

* `write_version` is the plugin's per-process counter; backfilled rows use the
  transaction index instead (same order within a slot).
* `index` comes from `transactionIndex` (agave 4.3 RPC). An older RPC that does
  not return it gives `index = -1`.
* Token balance `ui_token_amount`: the ledger stores a zero (and sub-1e-16)
  amount as `uiAmount: null`; it is recomputed as `amount / 10^decimals`, which
  is what the validator reported (checked against rows of the old run).
* `slot.parent` is NULL unless `--with-blocks` is given; only slots with a
  stored transaction get a `slot` row. `--with-blocks` fetches `getBlock`
  (no transactions, no rewards) once per slot and also writes the `block` row
  (blockhash, block_time, block_height; `rewards` empty). Decide on
  `--with-blocks` before the first fetch: a slot already written as `rooted`
  keeps its parent-less row (the rooted guard).
* Transactions whose version the RPC cannot return are recorded as
  `unsupported_version` in `done.tsv` and reported (v1 transactions are fetched
  with `maxSupportedTransactionVersion: 1` and stored with `message_type` 2).

Build on mainnet-la with a separate target directory: the `backfill` feature
also rebuilds the plugin library with extra reqwest features, and the deployed
`target/release/libsolana_geyser_plugin_postgres.so` should stay the default
build.

```
cd /mnt/solana/build/solana-accountsdb-plugin-postgres
CARGO_TARGET_DIR=target-backfill cargo build --release --locked --features backfill --bin pg_backfill
cargo test --locked --features backfill        # optional, uses the fixtures only
```

Run (in tmux or with nohup; the log is also written to
`<work-dir>/backfill.log`). Mentions and `store_log_messages: false` match the
live config; the connection uses the plugin's `host`/`user`/`port`
(rust-postgres does not read `~/.pgpass`, so a password must be in the
connection string; it is redacted from all output):

```
jq -r '.transaction_selector.mentions[]' ~/bin/plugin-pg.json > ~/pg-backfill/mentions.txt
export PG_BACKFILL_DB="host=127.0.0.1 user=solana port=5432 dbname=solana"
target-backfill/release/pg_backfill \
    --work-dir ~/pg-backfill/gap --mentions-file ~/pg-backfill/mentions.txt \
    --from-slot 422906791 --to-slot 452946630 --phase all \
    --rpc https://api.mainnet-beta.solana.com --rps 1 \
    --store-log-messages false
```

* `--rpc [RPS@]URL` is repeatable; every endpoint has its own rate limit
  (`--rps` unless given as prefix, e.g. `--rpc 40@https://my-rpc.example/KEY`).
  Requests go to whichever endpoint is free first; a 429 pauses only that
  endpoint (`Retry-After` is honored) and the request is retried, other
  failures are retried with backoff up to `--max-retries`. URLs are printed as
  `scheme://host` only, so API keys stay out of the log.
* Progress is printed every `--progress-secs`: finished/total, outcome counts,
  recent and overall rate, ETA, and per-endpoint request/429/error counts.
* `--dry-run --limit 50` fetches and converts without touching the database
  or `done.tsv` and prints each row (`--dump-rows FILE` writes them as JSON in
  the shape of `row_to_json`).
* `--phase collect` / `--phase fetch` run one phase; `fetch` can be restarted
  with other endpoints or rates at any time.

Expected duration for the measured gap volume (about 500k unique signatures:
470k for `55NQkF...`, 26k for `Avgu2v...`, under 15k for the other 23):

* The public endpoint allows 10 `getTransaction` and 10
  `getSignaturesForAddress` calls per 10 s per IP (`x-ratelimit-method-limit`,
  free tier), i.e. 1 request/s: collect is about 520 pages (~10 minutes),
  fetch about 500k requests, **~140 hours (~6 days)**. A higher `--rps` only
  produces 429s on that endpoint.
* With endpoints adding up to 10 requests/s: ~14 hours; 50 requests/s: ~3
  hours.
* `--with-blocks` adds one `getBlock` per distinct slot (most of these
  transactions are in different slots, so up to twice the requests; the public
  endpoint allows only 6 `getBlock` per 10 s).

Checks after the run (read-only):

```
SELECT count(*), min(slot), max(slot) FROM transaction WHERE slot BETWEEN 422906791 AND 452946630;
SELECT count(*) FROM transaction WHERE slot BETWEEN 422906791 AND 452946630 AND index = -1;
cut -f2 ~/pg-backfill/gap/done.tsv | sort | uniq -c       # outcomes
```

### Configuration File Format

The plugin is configured using the input configuration file. An example
configuration file looks like the following:

```
{
	"libpath": "/solana/target/release/libsolana_geyser_plugin_postgres.so",
	"host": "postgres-server",
	"user": "solana",
	"port": 5433,
	"threads": 20,
	"batch_size": 20,
	"panic_on_db_errors": true,
	"accounts_selector" : {
		"accounts" : ["*"]
	}
}
```

The `host`, `user`, and `port` control the PostgreSQL configuration
information. For more advanced connection options, please use the
`connection_str` field. Please see [Rust Postgres Configuration](https://docs.rs/postgres/0.19.2/postgres/config/struct.Config.html).

To improve the throughput to the database, the plugin supports connection pooling
using multiple threads, each maintaining a connection to the PostgreSQL database.
The count of the threads is controlled by the `threads` field. A higher thread
count usually offers better performance.

To further improve performance when saving large numbers of accounts at
startup, the plugin uses bulk inserts. The batch size is controlled by the
`batch_size` parameter. This can help reduce the round trips to the database.

The `panic_on_db_errors` can be used to panic the validator in case of database
errors to ensure data consistency.

### Support Connection Using SSL

To connect to the PostgreSQL database via SSL, set `use_ssl` to true, and specify
the server certificate, the client certificate and the client key files in PEM format
using the `server_ca`, `client_cert` and `client_key` fields respectively.
For example:

```
    "use_ssl": true,
    "server_ca": "/solana/.ssh/server-ca.pem",
    "client_cert": "/solana/.ssh/client-cert.pem",
    "client_key": "/solana/.ssh/client-key.pem",
```

### Account Selection

The `accounts_selector` can be used to filter the accounts that should be persisted.

For example, one can use the following to persist only the accounts with particular
Base58-encoded Pubkeys,

```
    "accounts_selector" : {
         "accounts" : ["pubkey-1", "pubkey-2", ..., "pubkey-n"],
    }
```

Or use the following to select accounts with certain program owners:

```
    "accounts_selector" : {
         "owners" : ["pubkey-owner-1", "pubkey-owner-2", ..., "pubkey-owner-m"],
    }
```

To select all accounts, use the wildcard character (*):

```
    "accounts_selector" : {
         "accounts" : ["*"],
    }
```

### Transaction Selection

`transaction_selector`, controls if and what transactions to store.
If this field is missing, none of the transactions are stored.

For example, one can use the following to select only the transactions
referencing accounts with particular Base58-encoded Pubkeys,

```
"transaction_selector" : {
    "mentions" : \["pubkey-1", "pubkey-2", ..., "pubkey-n"\],
}
```

The `mentions` field supports wildcards to select all transaction or
all 'vote' transactions. For example, to select all transactions:

```
"transaction_selector" : {
    "mentions" : \["*"\],
}
```

To select all vote transactions:

```
"transaction_selector" : {
    "mentions" : \["all_votes"\],
}
```

### Database Setup

#### Install PostgreSQL Server

Please follow [PostgreSQL Ubuntu Installation](https://www.postgresql.org/download/linux/ubuntu/)
on instructions to install the PostgreSQL database server. For example, to
install postgresql-14,

```
sudo sh -c 'echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" > /etc/apt/sources.list.d/pgdg.list'
wget --quiet -O - https://www.postgresql.org/media/keys/ACCC4CF8.asc | sudo apt-key add -
sudo apt-get update
sudo apt-get -y install postgresql-14
```
#### Control the Database Access

Modify the pg_hba.conf as necessary to grant the plugin to access the database.
For example, in /etc/postgresql/14/main/pg_hba.conf, the following entry allows
nodes with IPs in the CIDR 10.138.0.0/24 to access all databases. The validator
runs in a node with an ip in the specified range.

```
host    all             all             10.138.0.0/24           trust
```

It is recommended to run the database server on a separate node from the validator for
better performance.

#### Configure the Database Performance Parameters

Please refer to the [PostgreSQL Server Configuration](https://www.postgresql.org/docs/14/runtime-config.html)
for configuration details. The referential implementation uses the following
configurations for better database performance in the /etc/postgresql/14/main/postgresql.conf
which are different from the default postgresql-14 installation.

```
max_connections = 200                  # (change requires restart)
shared_buffers = 1GB                   # min 128kB
effective_io_concurrency = 1000        # 1-1000; 0 disables prefetching
wal_level = minimal                    # minimal, replica, or logical
fsync = off                            # flush data to disk for crash safety
synchronous_commit = off               # synchronization level;
full_page_writes = off                 # recover from partial page writes
max_wal_senders = 0                    # max number of walsender processes
```

The sample scripts/postgresql.conf can be used for reference.

#### Create the Database Instance and the Role

Start the server:

```
sudo systemctl start postgresql@14-main
```

Create the database. For example, the following creates a database named 'solana':

```
sudo -u postgres createdb solana -p 5433
```

Create the database user. For example, the following creates a regular user named 'solana':

```
sudo -u postgres createuser -p 5433 solana
```

Verify the database is working using psql. For example, assuming the node running
PostgreSQL has the ip 10.138.0.9, the following command will land in a shell where
SQL commands can be entered:

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana
```

#### Create the Schema Objects

Use the scripts/create_schema.sql

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana -f scripts/create_schema.sql
```

After this, start the validator with the plugin by using the `--geyser-plugin-config`
argument mentioned above.

#### Destroy the Schema Objects

To destroy the database objects, created by `create_schema.sql`, use
drop_schema.sql. For example,

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana -f scripts/drop_schema.sql
```

### Capture Historical Account Data

To capture account historical data, in the configuration file, turn
`store_account_historical_data` to true.

And ensure the database trigger is created to save data in the `audit_table` when
records in `account` are updated, as shown in `create_schema.sql`,

```
CREATE FUNCTION audit_account_update() RETURNS trigger AS $audit_account_update$
    BEGIN
		INSERT INTO account_audit (pubkey, owner, lamports, slot, executable, rent_epoch, data, write_version, updated_on)
            VALUES (OLD.pubkey, OLD.owner, OLD.lamports, OLD.slot,
                    OLD.executable, OLD.rent_epoch, OLD.data, OLD.write_version, OLD.updated_on);
        RETURN NEW;
    END;

$audit_account_update$ LANGUAGE plpgsql;

CREATE TRIGGER account_update_trigger AFTER UPDATE OR DELETE ON account
    FOR EACH ROW EXECUTE PROCEDURE audit_account_update();
```

The trigger can be dropped to disable this feature, for example,

```
DROP TRIGGER account_update_trigger ON account;
```

Over time, the account_audit can accumulate large amount of data. You may choose to
limit that by deleting older historical data.

For example, the following SQL statement can be used to keep up to 1000 of the most
recent records for an account:

```
delete from account_audit a2 where (pubkey, write_version) in
    (select pubkey, write_version from
        (select a.pubkey, a.updated_on, a.slot, a.write_version, a.lamports,
            rank() OVER ( partition by pubkey order by write_version desc) as rnk
            from account_audit a) ranked
            where ranked.rnk > 1000)
```

### Main Tables

The following are the tables in the Postgres database

| Table         | Description             |
|:--------------|:------------------------|
| account       | Account data            |
| block         | Block metadata          |
| slot          | Slot metadata           |
| transaction   | Transaction data        |
| account_audit | Account historical data |


### Performance Considerations

When a validator lacks sufficient computing power, the overhead of saving the
account data can cause it to fall behind the network especially when all
accounts or a large number of accounts are selected. The node hosting the
PostgreSQL database needs to be powerful enough to handle the database loads
as well. It has been found using GCP n2-standard-64 machine type for the
validator and n2-highmem-32 for the PostgreSQL node is adequate for handling
transmitting all accounts while keeping up with the network. In addition, it is
best to keep the validator and the PostgreSQL in the same local network to
reduce latency. You may need to size the validator and database nodes
differently if serving other loads.
