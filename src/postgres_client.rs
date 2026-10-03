#![allow(clippy::arithmetic_side_effects)]

mod postgres_client_account_index;
pub mod postgres_client_block_metadata;
pub mod postgres_client_transaction;

/// A concurrent implementation for writing accounts into the PostgreSQL in parallel.
use {
    crate::{
        geyser_plugin_postgres::{GeyserPluginPostgresConfig, GeyserPluginPostgresError},
        postgres_client::postgres_client_account_index::TokenSecondaryIndexEntry,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPluginError, ReplicaAccountInfo, ReplicaAccountInfoV2, ReplicaAccountInfoV3,
        SlotStatus,
    },
    chrono::Utc,
    crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender},
    log::*,
    openssl::ssl::{SslConnector, SslFiletype, SslMethod},
    postgres::{types, Client, NoTls, Statement},
    postgres_openssl::MakeTlsConnector,
    std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

pub use {
    postgres_client_block_metadata::DbBlockInfo,
    postgres_client_transaction::{LogTransactionRequest, TransactionNotification},
};

/// The maximum asynchronous requests allowed in the channel to avoid excessive
/// memory usage. The downside -- calls after this threshold is reached can get blocked.
const MAX_ASYNC_REQUESTS: usize = 40960;
const SAFE_BATCH_STARTING_SLOT_CUSHION: u64 = 2 * 40960;
const DEFAULT_POSTGRES_PORT: u16 = 5432;
const DEFAULT_THREADS_COUNT: usize = 100;
const DEFAULT_ACCOUNTS_INSERT_BATCH_SIZE: usize = 10;
const ACCOUNT_COLUMN_COUNT: usize = 10;
const DEFAULT_PANIC_ON_DB_ERROR: bool = false;
const DEFAULT_STORE_LOG_MESSAGES: bool = true;
const DEFAULT_PANIC_ON_ROW_ERRORS: bool = false;
const DEFAULT_STORE_ACCOUNT_HISTORICAL_DATA: bool = false;

struct PostgresSqlClientWrapper {
    client: Client,
    update_account_stmt: Statement,
    bulk_account_insert_stmt: Statement,
    update_slot_with_parent_stmt: Statement,
    update_slot_without_parent_stmt: Statement,
    update_transaction_log_stmt: Statement,
    update_block_metadata_stmt: Statement,
    insert_account_audit_stmt: Option<Statement>,
    insert_token_owner_index_stmt: Option<Statement>,
    insert_token_mint_index_stmt: Option<Statement>,
    bulk_insert_token_owner_index_stmt: Option<Statement>,
    bulk_insert_token_mint_index_stmt: Option<Statement>,
}

pub struct SimplePostgresClient {
    batch_size: usize,
    slots_at_startup: HashSet<u64>,
    pending_account_updates: Vec<DbAccountInfo>,
    index_token_owner: bool,
    index_token_mint: bool,
    pending_token_owner_index: Vec<TokenSecondaryIndexEntry>,
    pending_token_mint_index: Vec<TokenSecondaryIndexEntry>,
    client: Mutex<PostgresSqlClientWrapper>,
}

struct PostgresClientWorker {
    client: SimplePostgresClient,
    /// Indicating if accounts notification during startup is done.
    is_startup_done: bool,
}

impl Eq for DbAccountInfo {}

#[derive(Clone, PartialEq, Debug)]
pub struct DbAccountInfo {
    pub pubkey: Vec<u8>,
    pub lamports: i64,
    pub owner: Vec<u8>,
    pub executable: bool,
    pub rent_epoch: i64,
    pub data: Vec<u8>,
    pub slot: i64,
    pub write_version: i64,
    pub txn_signature: Option<Vec<u8>>,
}

pub(crate) fn abort() -> ! {
    #[cfg(not(test))]
    {
        // standard error is usually redirected to a log file, cry for help on standard output as
        // well
        eprintln!("Validator process aborted. The validator log may contain further details");
        std::process::exit(1);
    }

    #[cfg(test)]
    panic!("process::exit(1) is intercepted for friendly test failure...");
}

/// Returns true when the database rejected the values of a single statement
/// (SQLSTATE class 22 "data exception" or 23 "integrity constraint violation"),
/// e.g. an enum label or a string length the schema does not accept. Such an
/// error affects only that row; the connection and the remaining rows are fine.
fn is_row_level_error(err: &postgres::Error) -> bool {
    err.code()
        .map(|code| {
            let code = code.code();
            code.starts_with("22") || code.starts_with("23")
        })
        .unwrap_or(false)
}

/// Converts a failed write into a plugin error. Row-level errors are tagged as
/// `GeyserPluginPostgresError::RowDataError` so that the worker can skip the
/// row instead of aborting the validator; see `is_row_data_error`.
pub(crate) fn db_write_error(context: &str, err: postgres::Error) -> GeyserPluginError {
    let msg = format!("Failed to persist {context} to the PostgreSQL database. Error: {err:?}");
    error!("{}", msg);
    if is_row_level_error(&err) {
        GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::RowDataError { msg }))
    } else {
        GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataStoreWriteError {
            msg,
        }))
    }
}

fn is_row_data_error(err: &GeyserPluginError) -> bool {
    match err {
        GeyserPluginError::Custom(err) => matches!(
            err.downcast_ref::<GeyserPluginPostgresError>(),
            Some(GeyserPluginPostgresError::RowDataError { .. })
        ),
        _ => false,
    }
}

/// Minimal replacement for `solana_sdk::timing::AtomicInterval`.
struct ReportInterval {
    start: Instant,
    last_ms: AtomicU64,
}

impl Default for ReportInterval {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }
}

impl ReportInterval {
    fn should_update(&self, interval_ms: u64) -> bool {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last_ms.load(Ordering::Relaxed);
        now.saturating_sub(last) >= interval_ms
            && self
                .last_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

impl DbAccountInfo {
    fn new<T: ReadableAccountInfo>(account: &T, slot: u64) -> DbAccountInfo {
        let data = account.data().to_vec();
        Self {
            pubkey: account.pubkey().to_vec(),
            lamports: account.lamports(),
            owner: account.owner().to_vec(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
            data,
            slot: slot as i64,
            write_version: account.write_version(),
            txn_signature: account.txn_signature().map(|v| v.to_vec()),
        }
    }
}

pub trait ReadableAccountInfo: Sized {
    fn pubkey(&self) -> &[u8];
    fn owner(&self) -> &[u8];
    fn lamports(&self) -> i64;
    fn executable(&self) -> bool;
    fn rent_epoch(&self) -> i64;
    fn data(&self) -> &[u8];
    fn write_version(&self) -> i64;
    fn txn_signature(&self) -> Option<&[u8]>;
}

impl ReadableAccountInfo for DbAccountInfo {
    fn pubkey(&self) -> &[u8] {
        &self.pubkey
    }

    fn owner(&self) -> &[u8] {
        &self.owner
    }

    fn lamports(&self) -> i64 {
        self.lamports
    }

    fn executable(&self) -> bool {
        self.executable
    }

    fn rent_epoch(&self) -> i64 {
        self.rent_epoch
    }

    fn data(&self) -> &[u8] {
        &self.data
    }

    fn write_version(&self) -> i64 {
        self.write_version
    }

    fn txn_signature(&self) -> Option<&[u8]> {
        self.txn_signature.as_deref()
    }
}

macro_rules! impl_readable_replica_account_info {
    ($ty:ident, |$this:ident| $txn_signature:expr) => {
        impl ReadableAccountInfo for $ty<'_> {
            fn pubkey(&self) -> &[u8] {
                self.pubkey
            }

            fn owner(&self) -> &[u8] {
                self.owner
            }

            fn lamports(&self) -> i64 {
                self.lamports as i64
            }

            fn executable(&self) -> bool {
                self.executable
            }

            fn rent_epoch(&self) -> i64 {
                self.rent_epoch as i64
            }

            fn data(&self) -> &[u8] {
                self.data
            }

            fn write_version(&self) -> i64 {
                self.write_version as i64
            }

            fn txn_signature(&self) -> Option<&[u8]> {
                let $this = self;
                $txn_signature
            }
        }
    };
}

impl_readable_replica_account_info!(ReplicaAccountInfo, |_this| None);
impl_readable_replica_account_info!(ReplicaAccountInfoV2, |this| this
    .txn_signature
    .map(|signature| signature.as_ref()));
impl_readable_replica_account_info!(ReplicaAccountInfoV3, |this| this
    .txn
    .map(|txn| txn.signature().as_ref()));

pub trait PostgresClient {
    fn join(&mut self) -> thread::Result<()> {
        Ok(())
    }

    fn update_account(
        &mut self,
        account: DbAccountInfo,
        is_startup: bool,
    ) -> Result<(), GeyserPluginError>;

    fn update_slot_status(
        &mut self,
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
    ) -> Result<(), GeyserPluginError>;

    fn notify_end_of_startup(&mut self) -> Result<(), GeyserPluginError>;

    fn log_transaction(
        &mut self,
        transaction_log_info: LogTransactionRequest,
    ) -> Result<(), GeyserPluginError>;

    fn update_block_metadata(
        &mut self,
        block_info: UpdateBlockMetadataRequest,
    ) -> Result<(), GeyserPluginError>;
}

impl SimplePostgresClient {
    pub fn connect_to_db(config: &GeyserPluginPostgresConfig) -> Result<Client, GeyserPluginError> {
        let port = config.port.unwrap_or(DEFAULT_POSTGRES_PORT);

        let connection_str = if let Some(connection_str) = &config.connection_str {
            connection_str.clone()
        } else {
            if config.host.is_none() || config.user.is_none() {
                let msg = format!(
                    "\"connection_str\": {:?}, or \"host\": {:?} \"user\": {:?} must be specified",
                    config.connection_str, config.host, config.user
                );
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            format!(
                "host={} user={} port={}",
                config.host.as_ref().unwrap(),
                config.user.as_ref().unwrap(),
                port
            )
        };

        let result = if let Some(true) = config.use_ssl {
            if config.server_ca.is_none() {
                let msg = "\"server_ca\" must be specified when \"use_ssl\" is set".to_string();
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            if config.client_cert.is_none() {
                let msg = "\"client_cert\" must be specified when \"use_ssl\" is set".to_string();
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            if config.client_key.is_none() {
                let msg = "\"client_key\" must be specified when \"use_ssl\" is set".to_string();
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
            if let Err(err) = builder.set_ca_file(config.server_ca.as_ref().unwrap()) {
                let msg = format!(
                    "Failed to set the server certificate specified by \"server_ca\": {}. Error: ({})",
                    config.server_ca.as_ref().unwrap(), err);
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            if let Err(err) =
                builder.set_certificate_file(config.client_cert.as_ref().unwrap(), SslFiletype::PEM)
            {
                let msg = format!(
                    "Failed to set the client certificate specified by \"client_cert\": {}. Error: ({})",
                    config.client_cert.as_ref().unwrap(), err);
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }
            if let Err(err) =
                builder.set_private_key_file(config.client_key.as_ref().unwrap(), SslFiletype::PEM)
            {
                let msg = format!(
                    "Failed to set the client key specified by \"client_key\": {}. Error: ({})",
                    config.client_key.as_ref().unwrap(),
                    err
                );
                return Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::ConfigurationError { msg },
                )));
            }

            let mut connector = MakeTlsConnector::new(builder.build());
            connector.set_callback(|connect_config, _domain| {
                connect_config.set_verify_hostname(false);
                Ok(())
            });
            Client::connect(&connection_str, connector)
        } else {
            Client::connect(&connection_str, NoTls)
        };

        match result {
            Err(err) => {
                let msg = format!(
                    "Error in connecting to the PostgreSQL database: {:?} connection_str: {:?}",
                    err, connection_str
                );
                error!("{}", msg);
                Err(GeyserPluginError::Custom(Box::new(
                    GeyserPluginPostgresError::DataStoreConnectionError { msg },
                )))
            }
            Ok(client) => Ok(client),
        }
    }

    fn build_bulk_account_insert_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        let batch_size = config
            .batch_size
            .unwrap_or(DEFAULT_ACCOUNTS_INSERT_BATCH_SIZE);
        let mut stmt = String::from("INSERT INTO account AS acct (pubkey, slot, owner, lamports, executable, rent_epoch, data, write_version, updated_on, txn_signature) VALUES");
        for j in 0..batch_size {
            let row = j * ACCOUNT_COLUMN_COUNT;
            let val_str = format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
                row + 1,
                row + 2,
                row + 3,
                row + 4,
                row + 5,
                row + 6,
                row + 7,
                row + 8,
                row + 9,
                row + 10,
            );

            if j == 0 {
                stmt = format!("{} {}", &stmt, val_str);
            } else {
                stmt = format!("{}, {}", &stmt, val_str);
            }
        }

        let handle_conflict = "ON CONFLICT (pubkey) DO UPDATE SET slot=excluded.slot, owner=excluded.owner, lamports=excluded.lamports, executable=excluded.executable, rent_epoch=excluded.rent_epoch, \
            data=excluded.data, write_version=excluded.write_version, updated_on=excluded.updated_on, txn_signature=excluded.txn_signature WHERE acct.slot < excluded.slot OR (\
            acct.slot = excluded.slot AND acct.write_version < excluded.write_version)";

        stmt = format!("{} {}", stmt, handle_conflict);

        info!("{}", stmt);
        let bulk_stmt = client.prepare(&stmt);

        match bulk_stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the accounts update PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(update_account_stmt) => Ok(update_account_stmt),
        }
    }

    fn build_single_account_upsert_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        let stmt = "INSERT INTO account AS acct (pubkey, slot, owner, lamports, executable, rent_epoch, data, write_version, updated_on, txn_signature) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
        ON CONFLICT (pubkey) DO UPDATE SET slot=excluded.slot, owner=excluded.owner, lamports=excluded.lamports, executable=excluded.executable, rent_epoch=excluded.rent_epoch, \
        data=excluded.data, write_version=excluded.write_version, updated_on=excluded.updated_on, txn_signature=excluded.txn_signature  WHERE acct.slot < excluded.slot OR (\
        acct.slot = excluded.slot AND acct.write_version < excluded.write_version)";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the accounts update PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(update_account_stmt) => Ok(update_account_stmt),
        }
    }

    fn prepare_query_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
        stmt: &str,
    ) -> Result<Statement, GeyserPluginError> {
        let statement = client.prepare(stmt);

        match statement {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the statement {} for PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        stmt, err, config.host, config.user, config
                    ),
                })))
            }
            Ok(statement) => Ok(statement),
        }
    }

    fn build_account_audit_insert_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        let stmt = "INSERT INTO account_audit (pubkey, slot, owner, lamports, executable, rent_epoch, data, write_version, updated_on, txn_signature) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the account_audit update PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(stmt) => Ok(stmt),
        }
    }

    fn build_slot_upsert_statement_with_parent(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        // A rooted slot is final: never let a late, lower status written by another
        // worker overwrite it (consumers select status = 'rooted').
        let stmt = "INSERT INTO slot AS s (slot, parent, status, updated_on) \
        VALUES ($1, $2, $3, $4) \
        ON CONFLICT (slot) DO UPDATE SET parent=excluded.parent, status=excluded.status, updated_on=excluded.updated_on \
        WHERE s.status <> 'rooted'";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the slot update PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(stmt) => Ok(stmt),
        }
    }

    fn build_slot_upsert_statement_without_parent(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        // See build_slot_upsert_statement_with_parent for the rooted guard.
        let stmt = "INSERT INTO slot AS s (slot, status, updated_on) \
        VALUES ($1, $2, $3) \
        ON CONFLICT (slot) DO UPDATE SET status=excluded.status, updated_on=excluded.updated_on \
        WHERE s.status <> 'rooted'";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the slot update PostgreSQL database: {} host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(stmt) => Ok(stmt),
        }
    }

    /// Internal function for inserting an account into account_audit table.
    fn insert_account_audit(
        account: &DbAccountInfo,
        statement: &Statement,
        client: &mut Client,
    ) -> Result<(), GeyserPluginError> {
        let lamports = account.lamports();
        let rent_epoch = account.rent_epoch();
        let updated_on = Utc::now().naive_utc();
        let result = client.execute(
            statement,
            &[
                &account.pubkey(),
                &account.slot,
                &account.owner(),
                &lamports,
                &account.executable(),
                &rent_epoch,
                &account.data(),
                &account.write_version(),
                &updated_on,
                &account.txn_signature(),
            ],
        );

        if let Err(err) = result {
            return Err(db_write_error("account_audit insert", err));
        }
        Ok(())
    }

    /// Internal function for updating or inserting a single account
    fn upsert_account_internal(
        account: &DbAccountInfo,
        statement: &Statement,
        client: &mut Client,
        insert_account_audit_stmt: &Option<Statement>,
        insert_token_owner_index_stmt: &Option<Statement>,
        insert_token_mint_index_stmt: &Option<Statement>,
    ) -> Result<(), GeyserPluginError> {
        let lamports = account.lamports();
        let rent_epoch = account.rent_epoch();
        let updated_on = Utc::now().naive_utc();
        let result = client.execute(
            statement,
            &[
                &account.pubkey(),
                &account.slot,
                &account.owner(),
                &lamports,
                &account.executable(),
                &rent_epoch,
                &account.data(),
                &account.write_version(),
                &updated_on,
                &account.txn_signature(),
            ],
        );

        match result {
            Err(err) => return Err(db_write_error("account upsert", err)),
            Ok(0) => {
                // If no records modified (inserted or updated), it is because the account is
                // updated at an older slot, insert the record directly into the account_audit
                // table.
                if let Some(statement) = insert_account_audit_stmt {
                    Self::insert_account_audit(account, statement, client)?;
                }
            }
            Ok(_) => {}
        }

        if let Some(insert_token_owner_index_stmt) = insert_token_owner_index_stmt {
            Self::update_token_owner_index(client, insert_token_owner_index_stmt, account)?;
        }

        if let Some(insert_token_mint_index_stmt) = insert_token_mint_index_stmt {
            Self::update_token_mint_index(client, insert_token_mint_index_stmt, account)?;
        }

        Ok(())
    }

    /// Update or insert a single account
    fn upsert_account(&mut self, account: &DbAccountInfo) -> Result<(), GeyserPluginError> {
        let client = self.client.get_mut().unwrap();
        let insert_account_audit_stmt = &client.insert_account_audit_stmt;
        let statement = &client.update_account_stmt;
        let insert_token_owner_index_stmt = &client.insert_token_owner_index_stmt;
        let insert_token_mint_index_stmt = &client.insert_token_mint_index_stmt;
        let client = &mut client.client;
        Self::upsert_account_internal(
            account,
            statement,
            client,
            insert_account_audit_stmt,
            insert_token_owner_index_stmt,
            insert_token_mint_index_stmt,
        )?;

        Ok(())
    }

    /// Insert accounts in batch to reduce network overhead
    fn insert_accounts_in_batch(
        &mut self,
        account: DbAccountInfo,
    ) -> Result<(), GeyserPluginError> {
        self.queue_secondary_indexes(&account);
        self.pending_account_updates.push(account);

        self.bulk_insert_accounts()?;
        self.bulk_insert_token_owner_index()?;
        self.bulk_insert_token_mint_index()
    }

    fn bulk_insert_accounts(&mut self) -> Result<(), GeyserPluginError> {
        if self.pending_account_updates.len() == self.batch_size {
            let mut values: Vec<&(dyn types::ToSql + Sync)> =
                Vec::with_capacity(self.batch_size * ACCOUNT_COLUMN_COUNT);
            let updated_on = Utc::now().naive_utc();
            for j in 0..self.batch_size {
                let account = &self.pending_account_updates[j];

                values.push(&account.pubkey);
                values.push(&account.slot);
                values.push(&account.owner);
                values.push(&account.lamports);
                values.push(&account.executable);
                values.push(&account.rent_epoch);
                values.push(&account.data);
                values.push(&account.write_version);
                values.push(&updated_on);
                values.push(&account.txn_signature);
            }
            let client = self.client.get_mut().unwrap();
            let result = client
                .client
                .query(&client.bulk_account_insert_stmt, &values);

            self.pending_account_updates.clear();

            if let Err(err) = result {
                return Err(db_write_error("account bulk insert", err));
            }
        }
        Ok(())
    }

    /// Flush any left over accounts in batch which are not processed in the last batch
    fn flush_buffered_writes(&mut self) -> Result<(), GeyserPluginError> {
        let client = self.client.get_mut().unwrap();
        let insert_account_audit_stmt = &client.insert_account_audit_stmt;
        let statement = &client.update_account_stmt;
        let insert_token_owner_index_stmt = &client.insert_token_owner_index_stmt;
        let insert_token_mint_index_stmt = &client.insert_token_mint_index_stmt;
        let insert_slot_stmt = &client.update_slot_without_parent_stmt;
        let client = &mut client.client;

        for account in self.pending_account_updates.drain(..) {
            Self::upsert_account_internal(
                &account,
                statement,
                client,
                insert_account_audit_stmt,
                insert_token_owner_index_stmt,
                insert_token_mint_index_stmt,
            )?;
        }

        let started = Instant::now();
        for slot in &self.slots_at_startup {
            Self::upsert_slot_status_internal(
                *slot,
                None,
                SlotStatus::Rooted,
                client,
                insert_slot_stmt,
            )?;
        }
        info!(
            "Flushed {} startup slots in {} us",
            self.slots_at_startup.len(),
            started.elapsed().as_micros()
        );

        self.slots_at_startup.clear();
        self.clear_buffered_indexes();
        Ok(())
    }

    fn upsert_slot_status_internal(
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
        client: &mut Client,
        statement: &Statement,
    ) -> Result<(), GeyserPluginError> {
        let slot = slot as i64; // postgres only supports i64
        let parent = parent.map(|parent| parent as i64);
        let updated_on = Utc::now().naive_utc();
        // Full status names; `slot.status` is widened to VARCHAR(32) by
        // scripts/migrate_agave_v4_3.sql.
        let status_str = status.as_str();

        let result = match parent {
            Some(parent) => client.execute(statement, &[&slot, &parent, &status_str, &updated_on]),
            None => client.execute(statement, &[&slot, &status_str, &updated_on]),
        };

        // Zero affected rows is expected when the slot is already rooted.
        result
            .map(|_| ())
            .map_err(|err| db_write_error(&format!("slot {slot} status {status_str}"), err))
    }

    pub fn new(config: &GeyserPluginPostgresConfig) -> Result<Self, GeyserPluginError> {
        info!("Creating SimplePostgresClient...");
        let mut client = Self::connect_to_db(config)?;
        let bulk_account_insert_stmt =
            Self::build_bulk_account_insert_statement(&mut client, config)?;
        let update_account_stmt = Self::build_single_account_upsert_statement(&mut client, config)?;

        let update_slot_with_parent_stmt =
            Self::build_slot_upsert_statement_with_parent(&mut client, config)?;
        let update_slot_without_parent_stmt =
            Self::build_slot_upsert_statement_without_parent(&mut client, config)?;
        let update_transaction_log_stmt =
            Self::build_transaction_info_upsert_statement(&mut client, config)?;
        let update_block_metadata_stmt =
            Self::build_block_metadata_upsert_statement(&mut client, config)?;

        let batch_size = config
            .batch_size
            .unwrap_or(DEFAULT_ACCOUNTS_INSERT_BATCH_SIZE);

        let store_account_historical_data = config
            .store_account_historical_data
            .unwrap_or(DEFAULT_STORE_ACCOUNT_HISTORICAL_DATA);

        let insert_account_audit_stmt = if store_account_historical_data {
            let stmt = Self::build_account_audit_insert_statement(&mut client, config)?;
            Some(stmt)
        } else {
            None
        };

        let bulk_insert_token_owner_index_stmt = if let Some(true) = config.index_token_owner {
            let stmt = Self::build_bulk_token_owner_index_insert_statement(&mut client, config)?;
            Some(stmt)
        } else {
            None
        };

        let bulk_insert_token_mint_index_stmt = if let Some(true) = config.index_token_mint {
            let stmt = Self::build_bulk_token_mint_index_insert_statement(&mut client, config)?;
            Some(stmt)
        } else {
            None
        };

        let insert_token_owner_index_stmt = if let Some(true) = config.index_token_owner {
            Some(Self::build_single_token_owner_index_upsert_statement(
                &mut client,
                config,
            )?)
        } else {
            None
        };

        let insert_token_mint_index_stmt = if let Some(true) = config.index_token_mint {
            Some(Self::build_single_token_mint_index_upsert_statement(
                &mut client,
                config,
            )?)
        } else {
            None
        };

        info!("Created SimplePostgresClient.");
        Ok(Self {
            batch_size,
            pending_account_updates: Vec::with_capacity(batch_size),
            client: Mutex::new(PostgresSqlClientWrapper {
                client,
                update_account_stmt,
                bulk_account_insert_stmt,
                update_slot_with_parent_stmt,
                update_slot_without_parent_stmt,
                update_transaction_log_stmt,
                update_block_metadata_stmt,
                insert_account_audit_stmt,
                insert_token_owner_index_stmt,
                insert_token_mint_index_stmt,
                bulk_insert_token_owner_index_stmt,
                bulk_insert_token_mint_index_stmt,
            }),
            index_token_owner: config.index_token_owner.unwrap_or_default(),
            index_token_mint: config.index_token_mint.unwrap_or(false),
            pending_token_owner_index: Vec::with_capacity(batch_size),
            pending_token_mint_index: Vec::with_capacity(batch_size),
            slots_at_startup: HashSet::default(),
        })
    }

    fn get_highest_available_slot(&mut self) -> Result<u64, GeyserPluginError> {
        let client = self.client.get_mut().unwrap();

        let last_slot_query = "SELECT slot FROM slot ORDER BY slot DESC LIMIT 1;";

        let result = client.client.query_opt(last_slot_query, &[]);
        match result {
            Ok(opt_slot) => Ok(opt_slot
                .map(|row| {
                    let raw_slot: i64 = row.get(0);
                    raw_slot as u64
                })
                .unwrap_or(0)),
            Err(err) => {
                let msg = format!(
                    "Failed to receive last slot from PostgreSQL database. Error: {:?}",
                    err
                );
                error!("{}", msg);
                Err(GeyserPluginError::AccountsUpdateError { msg })
            }
        }
    }
}

impl PostgresClient for SimplePostgresClient {
    fn update_account(
        &mut self,
        account: DbAccountInfo,
        is_startup: bool,
    ) -> Result<(), GeyserPluginError> {
        trace!(
            "Updating account {} with owner {} at slot {}",
            bs58::encode(account.pubkey()).into_string(),
            bs58::encode(account.owner()).into_string(),
            account.slot,
        );
        if !is_startup {
            return self.upsert_account(&account);
        }

        self.slots_at_startup.insert(account.slot as u64);
        self.insert_accounts_in_batch(account)
    }

    fn update_slot_status(
        &mut self,
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
    ) -> Result<(), GeyserPluginError> {
        debug!("Updating slot {:?} at with status {:?}", slot, status);

        let client = self.client.get_mut().unwrap();

        let statement = match parent {
            Some(_) => &client.update_slot_with_parent_stmt,
            None => &client.update_slot_without_parent_stmt,
        };

        Self::upsert_slot_status_internal(slot, parent, status, &mut client.client, statement)
    }

    fn notify_end_of_startup(&mut self) -> Result<(), GeyserPluginError> {
        self.flush_buffered_writes()
    }

    fn log_transaction(
        &mut self,
        transaction_log_info: LogTransactionRequest,
    ) -> Result<(), GeyserPluginError> {
        self.log_transaction_impl(transaction_log_info)
    }

    fn update_block_metadata(
        &mut self,
        block_info: UpdateBlockMetadataRequest,
    ) -> Result<(), GeyserPluginError> {
        self.update_block_metadata_impl(block_info)
    }
}

struct UpdateAccountRequest {
    account: DbAccountInfo,
    is_startup: bool,
}

struct UpdateSlotRequest {
    slot: u64,
    parent: Option<u64>,
    slot_status: SlotStatus,
}

pub struct UpdateBlockMetadataRequest {
    pub block_info: DbBlockInfo,
}

#[warn(clippy::large_enum_variant)]
enum DbWorkItem {
    UpdateAccount(Box<UpdateAccountRequest>),
    UpdateSlot(Box<UpdateSlotRequest>),
    LogTransaction(Box<LogTransactionRequest>),
    UpdateBlockMetadata(Box<UpdateBlockMetadataRequest>),
}

impl PostgresClientWorker {
    fn new(config: GeyserPluginPostgresConfig) -> Result<Self, GeyserPluginError> {
        let result = SimplePostgresClient::new(&config);
        match result {
            Ok(client) => Ok(PostgresClientWorker {
                client,
                is_startup_done: false,
            }),
            Err(err) => {
                error!("Error in creating SimplePostgresClient: {}", err);
                Err(err)
            }
        }
    }

    fn handle_result(
        result: Result<(), GeyserPluginError>,
        what: &str,
        panic_on_db_errors: bool,
        panic_on_row_errors: bool,
    ) {
        if let Err(err) = result {
            let row_error = is_row_data_error(&err);
            error!(
                "Failed to {}{}: ({})",
                what,
                if row_error { " (row skipped)" } else { "" },
                err
            );
            if panic_on_db_errors && (!row_error || panic_on_row_errors) {
                abort();
            }
        }
    }

    fn do_work(
        &mut self,
        receiver: Receiver<DbWorkItem>,
        exit_worker: Arc<AtomicBool>,
        is_startup_done: Arc<AtomicBool>,
        startup_done_count: Arc<AtomicUsize>,
        panic_on_db_errors: bool,
        panic_on_row_errors: bool,
    ) -> Result<(), GeyserPluginError> {
        let handle = |result, what| {
            Self::handle_result(result, what, panic_on_db_errors, panic_on_row_errors)
        };
        while !exit_worker.load(Ordering::Relaxed) {
            let work = receiver.recv_timeout(Duration::from_millis(500));
            match work {
                Ok(work) => match work {
                    DbWorkItem::UpdateAccount(request) => handle(
                        self.client
                            .update_account(request.account, request.is_startup),
                        "update account",
                    ),
                    DbWorkItem::UpdateSlot(request) => handle(
                        self.client.update_slot_status(
                            request.slot,
                            request.parent,
                            request.slot_status,
                        ),
                        "update slot",
                    ),
                    DbWorkItem::LogTransaction(transaction_log_info) => handle(
                        self.client.log_transaction(*transaction_log_info),
                        "update transaction",
                    ),
                    DbWorkItem::UpdateBlockMetadata(block_info) => handle(
                        self.client.update_block_metadata(*block_info),
                        "update block metadata",
                    ),
                },
                Err(err) => match err {
                    RecvTimeoutError::Timeout => {
                        if !self.is_startup_done && is_startup_done.load(Ordering::Relaxed) {
                            handle(self.client.notify_end_of_startup(), "notify end of startup");
                            self.is_startup_done = true;
                            startup_done_count.fetch_add(1, Ordering::Relaxed);
                        }

                        continue;
                    }
                    _ => {
                        error!("Error in receiving the item {:?}", err);
                        if panic_on_db_errors {
                            abort();
                        }
                        break;
                    }
                },
            }
        }
        Ok(())
    }
}
pub struct ParallelPostgresClient {
    workers: Vec<JoinHandle<Result<(), GeyserPluginError>>>,
    exit_worker: Arc<AtomicBool>,
    is_startup_done: Arc<AtomicBool>,
    startup_done_count: Arc<AtomicUsize>,
    initialized_worker_count: Arc<AtomicUsize>,
    sender: Sender<DbWorkItem>,
    last_report: ReportInterval,
    transaction_write_version: AtomicU64,
    store_log_messages: bool,
}

impl ParallelPostgresClient {
    pub fn new(config: &GeyserPluginPostgresConfig) -> Result<Self, GeyserPluginError> {
        info!("Creating ParallelPostgresClient...");
        let (sender, receiver) = bounded(MAX_ASYNC_REQUESTS);
        let exit_worker = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::default();
        let is_startup_done = Arc::new(AtomicBool::new(false));
        let startup_done_count = Arc::new(AtomicUsize::new(0));
        let worker_count = config.threads.unwrap_or(DEFAULT_THREADS_COUNT);
        let initialized_worker_count = Arc::new(AtomicUsize::new(0));
        for i in 0..worker_count {
            let cloned_receiver = receiver.clone();
            let exit_clone = exit_worker.clone();
            let is_startup_done_clone = is_startup_done.clone();
            let startup_done_count_clone = startup_done_count.clone();
            let initialized_worker_count_clone = initialized_worker_count.clone();
            let config = config.clone();
            let worker = Builder::new()
                .name(format!("worker-{}", i))
                .spawn(move || -> Result<(), GeyserPluginError> {
                    let panic_on_db_errors = config
                        .panic_on_db_errors
                        .unwrap_or(DEFAULT_PANIC_ON_DB_ERROR);
                    let panic_on_row_errors = config
                        .panic_on_row_errors
                        .unwrap_or(DEFAULT_PANIC_ON_ROW_ERRORS);
                    let result = PostgresClientWorker::new(config);

                    match result {
                        Ok(mut worker) => {
                            initialized_worker_count_clone.fetch_add(1, Ordering::Relaxed);
                            worker.do_work(
                                cloned_receiver,
                                exit_clone,
                                is_startup_done_clone,
                                startup_done_count_clone,
                                panic_on_db_errors,
                                panic_on_row_errors,
                            )?;
                            Ok(())
                        }
                        Err(err) => {
                            error!("Error when making connection to database: ({})", err);
                            if panic_on_db_errors {
                                abort();
                            }
                            Err(err)
                        }
                    }
                })
                .unwrap();

            workers.push(worker);
        }

        info!("Created ParallelPostgresClient.");
        Ok(Self {
            last_report: ReportInterval::default(),
            workers,
            exit_worker,
            is_startup_done,
            startup_done_count,
            initialized_worker_count,
            sender,
            transaction_write_version: AtomicU64::default(),
            store_log_messages: config
                .store_log_messages
                .unwrap_or(DEFAULT_STORE_LOG_MESSAGES),
        })
    }

    /// True when no worker thread is running (e.g. none could connect), in which
    /// case nothing will ever drain the queue.
    fn all_workers_exited(&self) -> bool {
        self.workers.iter().all(|worker| worker.is_finished())
    }

    pub fn join(&mut self) -> thread::Result<()> {
        self.exit_worker.store(true, Ordering::Relaxed);
        while !self.workers.is_empty() {
            let worker = self.workers.pop();
            if worker.is_none() {
                break;
            }
            let worker = worker.unwrap();
            let result = worker.join().unwrap();
            if result.is_err() {
                error!("The worker thread has failed: {:?}", result);
            }
        }

        Ok(())
    }

    /// `has_txn` is false for account updates not caused by a transaction
    /// (accounts-db bookkeeping), which are skipped outside of startup.
    pub fn update_account<T: ReadableAccountInfo>(
        &self,
        account: &T,
        has_txn: bool,
        slot: u64,
        is_startup: bool,
    ) -> Result<(), GeyserPluginError> {
        if !is_startup && !has_txn {
            // we are not interested in accountsdb internal bookeeping updates
            return Ok(());
        }

        if self.last_report.should_update(30000) {
            debug!(
                "postgres-plugin-stats: message-queue-length={}",
                self.sender.len()
            );
        }
        let wrk_item = DbWorkItem::UpdateAccount(Box::new(UpdateAccountRequest {
            account: DbAccountInfo::new(account, slot),
            is_startup,
        }));

        if let Err(err) = self.sender.send(wrk_item) {
            return Err(GeyserPluginError::AccountsUpdateError {
                msg: format!(
                    "Failed to update the account {:?}, error: {:?}",
                    bs58::encode(account.pubkey()).into_string(),
                    err
                ),
            });
        }

        Ok(())
    }

    pub fn update_slot_status(
        &self,
        slot: u64,
        parent: Option<u64>,
        status: &SlotStatus,
    ) -> Result<(), GeyserPluginError> {
        if let Err(err) = self
            .sender
            .send(DbWorkItem::UpdateSlot(Box::new(UpdateSlotRequest {
                slot,
                parent,
                slot_status: status.clone(),
            })))
        {
            return Err(GeyserPluginError::SlotStatusUpdateError {
                msg: format!("Failed to update the slot {:?}, error: {:?}", slot, err),
            });
        }
        Ok(())
    }

    pub fn update_block_metadata(&self, block_info: DbBlockInfo) -> Result<(), GeyserPluginError> {
        let slot = block_info.slot;
        trace!(
            "queue block slot={} blockhash={} rewards={} block_time={:?} block_height={:?}",
            slot,
            block_info.blockhash,
            block_info.rewards.len(),
            block_info.block_time,
            block_info.block_height
        );
        if let Err(err) = self.sender.send(DbWorkItem::UpdateBlockMetadata(Box::new(
            UpdateBlockMetadataRequest { block_info },
        ))) {
            return Err(GeyserPluginError::SlotStatusUpdateError {
                msg: format!(
                    "Failed to update the block metadata at slot {:?}, error: {:?}",
                    slot, err
                ),
            });
        }
        Ok(())
    }

    pub fn notify_end_of_startup(&self) -> Result<(), GeyserPluginError> {
        info!("Notifying the end of startup");
        // Ensure all items in the queue has been received by the workers
        while !self.sender.is_empty() {
            if self.all_workers_exited() {
                error!("All PostgreSQL workers have exited; not waiting for the queue to drain");
                break;
            }
            sleep(Duration::from_millis(100));
        }
        self.is_startup_done.store(true, Ordering::Relaxed);

        // Wait for all worker threads to be done with flushing
        while self.startup_done_count.load(Ordering::Relaxed)
            < self.initialized_worker_count.load(Ordering::Relaxed)
        {
            if self.all_workers_exited() {
                error!("All PostgreSQL workers have exited; not waiting for the startup flush");
                break;
            }
            info!(
                "Startup done count: {}, good worker thread count: {}",
                self.startup_done_count.load(Ordering::Relaxed),
                self.initialized_worker_count.load(Ordering::Relaxed)
            );
            sleep(Duration::from_millis(100));
        }

        info!("Done with notifying the end of startup");
        Ok(())
    }
}

/// Enum labels and the `slot.status` width this build writes, which a database
/// created by an older create_schema.sql only has after
/// scripts/migrate_agave_v4_3.sql.
const REQUIRED_ENUM_LABELS: &[(&str, &[&str])] = &[
    (
        "\"TransactionErrorCode\"",
        &["ProgramCacheHitMaxLimit", "CommitCancelled", "Unknown"],
    ),
    ("\"RewardType\"", &["DeactivatedStake", "VATDebit"]),
];

fn longest_slot_status() -> usize {
    [
        SlotStatus::Processed,
        SlotStatus::Rooted,
        SlotStatus::Confirmed,
        SlotStatus::FirstShredReceived,
        SlotStatus::Completed,
        SlotStatus::CreatedBank,
        SlotStatus::Dead(String::new()),
    ]
    .iter()
    .map(|status| status.as_str().len())
    .max()
    .unwrap_or_default()
}

/// Returns the schema problems that would make every write of some kind fail.
fn check_schema(client: &mut Client) -> Result<Vec<String>, postgres::Error> {
    let mut problems = Vec::new();
    for (type_name, labels) in REQUIRED_ENUM_LABELS {
        let rows = client.query(
            &format!(
                "SELECT enumlabel::text FROM pg_enum WHERE enumtypid = '{type_name}'::regtype"
            ),
            &[],
        )?;
        let present: HashSet<String> = rows.iter().map(|row| row.get(0)).collect();
        for label in labels.iter().filter(|label| !present.contains(**label)) {
            problems.push(format!("enum {type_name} lacks label '{label}'"));
        }
    }
    let row = client.query_one(
        "SELECT atttypmod - 4 FROM pg_attribute WHERE attrelid = 'slot'::regclass AND attname = 'status'",
        &[],
    )?;
    let status_width: i32 = row.get(0);
    if status_width >= 0 && (status_width as usize) < longest_slot_status() {
        problems.push(format!(
            "slot.status is VARCHAR({status_width}), needs at least {}",
            longest_slot_status()
        ));
    }
    Ok(problems)
}

pub struct PostgresClientBuilder {}

impl PostgresClientBuilder {
    /// Refuses to load against a database that has not been migrated, instead of
    /// failing row by row later. Skipped when the database cannot be reached here;
    /// the workers then report the connection error according to the config.
    fn verify_schema(config: &GeyserPluginPostgresConfig) -> Result<(), GeyserPluginError> {
        let mut client = match SimplePostgresClient::connect_to_db(config) {
            Ok(client) => client,
            Err(err) => {
                warn!("Skipping the schema check, cannot connect: {}", err);
                return Ok(());
            }
        };
        match check_schema(&mut client) {
            Ok(problems) if problems.is_empty() => Ok(()),
            Ok(problems) => Err(GeyserPluginError::Custom(Box::new(
                GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "The database schema is older than this plugin ({}). \
                         Run scripts/migrate_agave_v4_3.sql first.",
                        problems.join("; ")
                    ),
                },
            ))),
            Err(err) => Err(GeyserPluginError::Custom(Box::new(
                GeyserPluginPostgresError::DataSchemaError {
                    msg: format!("Failed to check the database schema: {err}"),
                },
            ))),
        }
    }

    pub fn build_pararallel_postgres_client(
        config: &GeyserPluginPostgresConfig,
    ) -> Result<(ParallelPostgresClient, Option<u64>), GeyserPluginError> {
        Self::verify_schema(config)?;
        let batch_optimize_by_skiping_older_slots =
            match config.skip_upsert_existing_accounts_at_startup {
                true => {
                    let mut on_load_client = SimplePostgresClient::new(config)?;

                    // database if populated concurrently so we need to move some number of slots
                    // below highest available slot to make sure we do not skip anything that was already in DB.
                    let batch_slot_bound = on_load_client
                        .get_highest_available_slot()?
                        .saturating_sub(SAFE_BATCH_STARTING_SLOT_CUSHION);
                    info!(
                        "Set batch_optimize_by_skiping_older_slots to {}",
                        batch_slot_bound
                    );
                    Some(batch_slot_bound)
                }
                false => None,
            };

        ParallelPostgresClient::new(config).map(|v| (v, batch_optimize_by_skiping_older_slots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_longest_slot_status_fits_migration() {
        assert_eq!(longest_slot_status(), "first_shred_received".len());
        let migration = include_str!("../scripts/migrate_agave_v4_3.sql");
        assert!(migration.contains("ALTER COLUMN status TYPE VARCHAR(32)"));
        assert!(longest_slot_status() <= 32);
        for (type_name, labels) in REQUIRED_ENUM_LABELS {
            for label in *labels {
                assert!(migration.contains(&format!(
                    "ALTER TYPE {type_name} ADD VALUE IF NOT EXISTS '{label}'"
                )));
            }
        }
    }
}
