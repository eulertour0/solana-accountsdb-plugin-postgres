//! Backfills `transaction` (and `slot`, optionally `block`) rows for a slot range
//! the plugin did not see, from a Solana JSON-RPC node with transaction history.
//!
//! Phase "collect" pages `getSignaturesForAddress` for every mention of the
//! plugin's transaction selector and appends the deduplicated signatures inside
//! the slot range to `<work-dir>/queue.tsv`. Phase "fetch" streams that file,
//! fetches every signature with `getTransaction` (base64), converts the response
//! back into the native transaction + status meta the validator gives the plugin
//! (`rpc_convert`), builds the row with the plugin's `build_db_transaction` and
//! writes it with the plugin's prepared upsert statements. Both phases are
//! resumable: collect keeps a per-address cursor, fetch appends every finished
//! signature to `<work-dir>/done.tsv`. All writes are upserts, so re-running a
//! range never duplicates rows. See README "Backfilling a gap from RPC".

use {
    agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
    crossbeam_channel::{bounded, Receiver, Sender},
    serde::{Deserialize, Serialize},
    serde_json::{json, Value},
    solana_geyser_plugin_postgres::{
        geyser_plugin_postgres::GeyserPluginPostgresConfig,
        postgres_client::{
            check_schema, is_row_data_error,
            postgres_client_transaction::{
                DbCompiledInstruction, DbTransaction, DbTransactionMessageHeader,
                DbTransactionTokenBalance, LogTransactionRequest,
            },
            DbBlockInfo, PostgresClient, SimplePostgresClient, UpdateBlockMetadataRequest,
        },
        rpc_convert::{convert_rpc_transaction, NativeTransaction},
        transaction_selector::TransactionSelector,
    },
    solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta,
    std::{
        collections::{BTreeMap, HashSet, VecDeque},
        fs::{self, File, OpenOptions},
        io::{BufRead, BufReader, Write},
        path::{Path, PathBuf},
        process::exit,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Mutex, OnceLock,
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
};

const DEFAULT_RPC: &str = "https://api.mainnet-beta.solana.com";
const SIGNATURES_PAGE_LIMIT: usize = 1000;
/// The public mainnet endpoint allows 10 getTransaction / getSignaturesForAddress
/// (6 getBlock) calls per 10 s per IP (x-ratelimit-method-limit, free tier).
const DEFAULT_RPS: f64 = 1.0;
/// getTransaction returns `null` for a signature it cannot find (yet); a
/// signature listed by getSignaturesForAddress at `finalized` should exist, so
/// retry a few times (possibly on another endpoint) before giving up.
const NULL_RESULT_RETRIES: u32 = 3;
/// JSON-RPC errors that will not change on retry.
/// -32015: transaction version not supported, -32007: slot skipped,
/// -32009: missing in long-term storage, -326xx: malformed request.
const PERMANENT_RPC_ERRORS: [i64; 6] = [-32015, -32007, -32009, -32600, -32601, -32602];
const UNSUPPORTED_VERSION_ERROR: i64 = -32015;

const USAGE: &str = "\
pg_backfill: backfill the Geyser PostgreSQL plugin's tables from JSON-RPC history

USAGE:
  pg_backfill --work-dir DIR --from-slot N --to-slot N [OPTIONS]

  --phase collect|fetch|all   default all
  --mentions-file FILE        transaction_selector mentions, one base58 key per line
                              (required for collect and fetch; '#' comments allowed)
  --rpc [RPS@]URL             JSON-RPC endpoint, repeatable; each endpoint has its own
                              rate limit (RPS@ prefix, else --rps). Default public mainnet
  --rps N                     default requests/s per endpoint (default 1: the public
                              endpoint allows 10 getTransaction per 10 s per IP)
  --workers N                 concurrent requests (default 2 x total rps, 4..128)
  --db CONNSTR                PostgreSQL connection string (or env PG_BACKFILL_DB);
                              required for fetch unless --dry-run
  --dry-run                   fetch and convert, print rows, write nothing (no DB, no done file)
  --limit N                   fetch at most N pending signatures (useful with --dry-run)
  --store-log-messages B      true|false, default false (as the live plugin config)
  --with-blocks               also getBlock each touched slot: slot.parent + block row
                              (one extra request per distinct slot)
  --max-retries N             retries of a transient RPC failure (default 10; 429s are
                              retried without limit)
  --progress-secs N           progress interval (default 10)
  --verbose                   print every converted row in full with --dry-run
  --dump-rows FILE            with --dry-run, also write each row as JSON (the shape of
                              PostgreSQL row_to_json, for comparing with stored rows)
";

// ---------------------------------------------------------------------------
// Output: stdout + <work-dir>/backfill.log, with secrets redacted.

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static REDACTIONS: OnceLock<Vec<(String, String)>> = OnceLock::new();

fn redact(text: &str) -> String {
    let mut text = text.to_string();
    for (secret, replacement) in REDACTIONS.get().into_iter().flatten() {
        if !secret.is_empty() {
            text = text.replace(secret.as_str(), replacement);
        }
    }
    text
}

fn say(message: &str) {
    let line = format!(
        "{} {}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        redact(message)
    );
    println!("{line}");
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

macro_rules! say {
    ($($arg:tt)*) => { say(&format!($($arg)*)) };
}

fn die(message: &str) -> ! {
    say!("FATAL: {message}");
    exit(1)
}

/// Forwards the library's warnings and errors (e.g. database errors) to `say`.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            say!("[{}] {}", record.level(), record.args());
        }
    }
    fn flush(&self) {}
}

/// The password of a key/value (`password=...`) or URL (`postgres://u:p@h`)
/// connection string.
fn connection_password(connection: &str) -> Option<String> {
    if let Some(rest) = connection.split("://").nth(1) {
        let userinfo = rest.split('@').next().filter(|_| rest.contains('@'))?;
        return userinfo
            .split_once(':')
            .map(|(_, password)| password.to_string());
    }
    connection
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix("password="))
        .map(|password| password.trim_matches('\'').to_string())
}

/// `scheme://host[:port]` of a URL, so that API keys in paths or query strings
/// never reach the output.
fn display_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    format!("{scheme}://{host}")
}

// ---------------------------------------------------------------------------
// Command line.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Collect,
    Fetch,
    All,
}

struct Args {
    db: Option<String>,
    rpcs: Vec<(String, f64)>,
    from_slot: u64,
    to_slot: u64,
    mentions_file: Option<PathBuf>,
    work_dir: PathBuf,
    phase: Phase,
    dry_run: bool,
    limit: Option<u64>,
    store_log_messages: bool,
    with_blocks: bool,
    workers: Option<usize>,
    max_retries: u32,
    progress_secs: u64,
    verbose: bool,
    dump_rows: Option<PathBuf>,
}

fn parse_args() -> Args {
    fn value<T: FromStr>(name: &str, value: Option<String>) -> T {
        let value = value.unwrap_or_else(|| die(&format!("{name} needs a value\n{USAGE}")));
        value
            .parse()
            .unwrap_or_else(|_| die(&format!("invalid value for {name}: {value}")))
    }
    let mut db = std::env::var("PG_BACKFILL_DB").ok();
    let mut rpc_specs = Vec::new();
    let mut rps: f64 = DEFAULT_RPS;
    let (mut from_slot, mut to_slot, mut work_dir) = (None, None, None);
    let mut args = Args {
        db: None,
        rpcs: Vec::new(),
        from_slot: 0,
        to_slot: 0,
        mentions_file: None,
        work_dir: PathBuf::new(),
        phase: Phase::All,
        dry_run: false,
        limit: None,
        store_log_messages: false,
        with_blocks: false,
        workers: None,
        max_retries: 10,
        progress_secs: 10,
        verbose: false,
        dump_rows: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--db" => db = Some(value("--db", argv.next())),
            "--rpc" => rpc_specs.push(value::<String>("--rpc", argv.next())),
            "--rps" => rps = value("--rps", argv.next()),
            "--from-slot" => from_slot = Some(value("--from-slot", argv.next())),
            "--to-slot" => to_slot = Some(value("--to-slot", argv.next())),
            "--mentions-file" => {
                args.mentions_file = Some(value::<String>("--mentions-file", argv.next()).into())
            }
            "--work-dir" => work_dir = Some(value::<String>("--work-dir", argv.next())),
            "--phase" => {
                args.phase = match value::<String>("--phase", argv.next()).as_str() {
                    "collect" => Phase::Collect,
                    "fetch" => Phase::Fetch,
                    "all" => Phase::All,
                    other => die(&format!("invalid --phase {other}")),
                }
            }
            "--dry-run" => args.dry_run = true,
            "--limit" => args.limit = Some(value("--limit", argv.next())),
            "--store-log-messages" => {
                args.store_log_messages = value("--store-log-messages", argv.next())
            }
            "--with-blocks" => args.with_blocks = true,
            "--workers" => args.workers = Some(value("--workers", argv.next())),
            "--max-retries" => args.max_retries = value("--max-retries", argv.next()),
            "--progress-secs" => args.progress_secs = value("--progress-secs", argv.next()),
            "--verbose" => args.verbose = true,
            "--dump-rows" => {
                args.dump_rows = Some(value::<String>("--dump-rows", argv.next()).into())
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                exit(0)
            }
            other => die(&format!("unknown argument {other}\n{USAGE}")),
        }
    }
    if rpc_specs.is_empty() {
        rpc_specs.push(DEFAULT_RPC.to_string());
    }
    for spec in rpc_specs {
        // "40@https://host/..." sets the rate of one endpoint; a URL that itself
        // contains '@' (user info) is only split when the prefix is a number.
        let (rate, url): (f64, String) = match spec.split_once('@') {
            Some((rate, url)) if rate.parse::<f64>().is_ok() && url.contains("://") => {
                (rate.parse().unwrap(), url.to_string())
            }
            _ => (rps, spec),
        };
        if rate.is_nan() || rate <= 0.0 {
            die(&format!("rate of {} must be positive", display_url(&url)));
        }
        args.rpcs.push((url, rate));
    }
    args.db = db;
    args.from_slot = from_slot.unwrap_or_else(|| die(&format!("--from-slot is required\n{USAGE}")));
    args.to_slot = to_slot.unwrap_or_else(|| die(&format!("--to-slot is required\n{USAGE}")));
    args.work_dir = work_dir
        .unwrap_or_else(|| die(&format!("--work-dir is required\n{USAGE}")))
        .into();
    if args.from_slot > args.to_slot {
        die("--from-slot must not be above --to-slot");
    }
    if args.progress_secs == 0 {
        args.progress_secs = 1;
    }
    args
}

fn read_mentions(path: &Path) -> Vec<String> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| die(&format!("cannot read {}: {err}", path.display())));
    let mentions: Vec<String> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if mentions.is_empty() {
        die(&format!("{} lists no mentions", path.display()));
    }
    mentions
}

// ---------------------------------------------------------------------------
// JSON-RPC client with a rate limit per endpoint.

struct EndpointState {
    next_at: Instant,
    cooldown_until: Instant,
    consecutive_429: u32,
}

struct Endpoint {
    url: String,
    display: String,
    interval: Duration,
    requests: AtomicU64,
    rate_limited: AtomicU64,
    errors: AtomicU64,
}

#[derive(Debug)]
enum RpcFailure {
    /// A JSON-RPC error that retrying will not fix.
    Permanent { code: i64, message: String },
    /// Transient failures until `--max-retries` was exhausted.
    Exhausted(String),
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permanent { code, message } => write!(f, "RPC error {code}: {message}"),
            Self::Exhausted(message) => write!(f, "retries exhausted: {message}"),
        }
    }
}

struct RpcPool {
    client: reqwest::blocking::Client,
    endpoints: Vec<Endpoint>,
    state: Mutex<Vec<EndpointState>>,
    max_retries: u32,
}

fn jitter(max: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.subsec_nanos())
        .unwrap_or_default();
    max.mul_f64(f64::from(nanos % 1000) / 1000.0)
}

fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500).saturating_mul(1 << attempt.min(6));
    base.min(Duration::from_secs(30)) + jitter(Duration::from_millis(500))
}

impl RpcPool {
    fn new(rpcs: &[(String, f64)], max_retries: u32) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("pg_backfill/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|err| die(&format!("cannot build the HTTP client: {err}")));
        let now = Instant::now();
        Self {
            client,
            endpoints: rpcs
                .iter()
                .map(|(url, rate)| Endpoint {
                    url: url.clone(),
                    display: display_url(url),
                    interval: Duration::from_secs_f64(1.0 / rate),
                    requests: AtomicU64::default(),
                    rate_limited: AtomicU64::default(),
                    errors: AtomicU64::default(),
                })
                .collect(),
            state: Mutex::new(
                rpcs.iter()
                    .map(|_| EndpointState {
                        next_at: now,
                        cooldown_until: now,
                        consecutive_429: 0,
                    })
                    .collect(),
            ),
            max_retries,
        }
    }

    fn total_rps(&self) -> f64 {
        self.endpoints
            .iter()
            .map(|endpoint| 1.0 / endpoint.interval.as_secs_f64())
            .sum()
    }

    /// Reserves the earliest request slot over all endpoints and waits for it.
    fn acquire(&self) -> usize {
        let (index, wait) = {
            let mut state = self.state.lock().unwrap();
            let now = Instant::now();
            let (index, ready) = state
                .iter()
                .enumerate()
                .map(|(index, endpoint)| {
                    (
                        index,
                        now.max(endpoint.next_at).max(endpoint.cooldown_until),
                    )
                })
                .min_by_key(|(_, ready)| *ready)
                .unwrap();
            state[index].next_at = ready + self.endpoints[index].interval;
            (index, ready - now)
        };
        if !wait.is_zero() {
            thread::sleep(wait);
        }
        index
    }

    fn rate_limited(&self, index: usize, retry_after: Option<Duration>) {
        self.endpoints[index]
            .rate_limited
            .fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        let endpoint = &mut state[index];
        endpoint.consecutive_429 = endpoint.consecutive_429.saturating_add(1);
        let pause = retry_after.unwrap_or_else(|| backoff(endpoint.consecutive_429));
        endpoint.cooldown_until = endpoint.cooldown_until.max(Instant::now() + pause);
    }

    fn succeeded(&self, index: usize) {
        self.state.lock().unwrap()[index].consecutive_429 = 0;
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut attempt = 0;
        loop {
            let index = self.acquire();
            let endpoint = &self.endpoints[index];
            endpoint.requests.fetch_add(1, Ordering::Relaxed);
            let failure = match self.client.post(&endpoint.url).json(&body).send() {
                Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(|secs| Duration::from_secs(secs.min(120)));
                    self.rate_limited(index, retry_after);
                    continue;
                }
                Ok(response) if !response.status().is_success() => {
                    format!("HTTP {} from {}", response.status(), endpoint.display)
                }
                Ok(response) => match response.json::<Value>() {
                    Ok(mut reply) => {
                        if let Some(error) = reply.get("error") {
                            let code = error["code"].as_i64().unwrap_or_default();
                            let message = error["message"].as_str().unwrap_or("").to_string();
                            if PERMANENT_RPC_ERRORS.contains(&code) {
                                self.succeeded(index);
                                return Err(RpcFailure::Permanent { code, message });
                            }
                            format!("RPC error {code} from {}: {message}", endpoint.display)
                        } else {
                            self.succeeded(index);
                            return Ok(reply["result"].take());
                        }
                    }
                    Err(err) => format!("bad reply from {}: {err}", endpoint.display),
                },
                Err(err) => format!(
                    "request to {} failed: {}",
                    endpoint.display,
                    redact(&err.without_url().to_string())
                ),
            };
            endpoint.errors.fetch_add(1, Ordering::Relaxed);
            attempt += 1;
            if attempt > self.max_retries {
                return Err(RpcFailure::Exhausted(failure));
            }
            if attempt >= 3 {
                say!(
                    "{method}: {failure} (attempt {attempt}/{})",
                    self.max_retries
                );
            }
            thread::sleep(backoff(attempt));
        }
    }

    fn stats(&self) -> String {
        self.endpoints
            .iter()
            .map(|endpoint| {
                format!(
                    "{} req={} 429={} err={}",
                    endpoint.display,
                    endpoint.requests.load(Ordering::Relaxed),
                    endpoint.rate_limited.load(Ordering::Relaxed),
                    endpoint.errors.load(Ordering::Relaxed)
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

// ---------------------------------------------------------------------------
// Work directory.

/// Signatures are deduplicated by their first 16 bytes (8 MB per million).
fn signature_key(signature: &str) -> Option<u128> {
    let bytes = bs58::decode(signature).into_vec().ok()?;
    (bytes.len() == 64).then(|| u128::from_le_bytes(bytes[..16].try_into().unwrap()))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AddressCursor {
    /// `before` for the next getSignaturesForAddress page; None = newest.
    before: Option<String>,
    done: bool,
    pages: u64,
    found: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CollectState {
    from_slot: u64,
    to_slot: u64,
    addresses: BTreeMap<String, AddressCursor>,
}

struct WorkDir {
    root: PathBuf,
}

impl WorkDir {
    fn queue(&self) -> PathBuf {
        self.root.join("queue.tsv")
    }
    fn cursors(&self) -> PathBuf {
        self.root.join("cursors.json")
    }
    fn done(&self) -> PathBuf {
        self.root.join("done.tsv")
    }
    fn failed(&self) -> PathBuf {
        self.root.join("failed.tsv")
    }

    fn append(path: &Path) -> File {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|err| die(&format!("cannot open {}: {err}", path.display())))
    }

    fn load_state(&self, args: &Args) -> CollectState {
        let path = self.cursors();
        let state = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<CollectState>(&text)
                .unwrap_or_else(|err| die(&format!("{} is corrupt: {err}", path.display()))),
            Err(_) => CollectState {
                from_slot: args.from_slot,
                to_slot: args.to_slot,
                ..CollectState::default()
            },
        };
        if (state.from_slot, state.to_slot) != (args.from_slot, args.to_slot) {
            die(&format!(
                "{} was collected for slots {}..={}; use the same range or another --work-dir",
                self.root.display(),
                state.from_slot,
                state.to_slot
            ));
        }
        state
    }

    fn save_state(&self, state: &CollectState) {
        let path = self.cursors();
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(state).unwrap();
        let result = File::create(&tmp)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|_| fs::rename(&tmp, &path));
        if let Err(err) = result {
            die(&format!("cannot write {}: {err}", path.display()));
        }
    }

    /// Calls `f` for every well-formed line; a torn last line after a crash is
    /// ignored.
    fn for_each_line(path: &Path, mut f: impl FnMut(&[&str])) {
        let Ok(file) = File::open(path) else {
            return;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let fields: Vec<&str> = line.split('\t').collect();
            f(&fields);
        }
    }
}

/// One line of queue.tsv: slot, signature, transactionIndex ('-' if unknown),
/// failed (0/1), the address it was found through.
struct QueueEntry {
    slot: u64,
    signature: String,
    index: Option<u32>,
}

fn parse_queue_line(fields: &[&str]) -> Option<QueueEntry> {
    if fields.len() < 5 {
        return None;
    }
    signature_key(fields[1])?;
    Some(QueueEntry {
        slot: fields[0].parse().ok()?,
        signature: fields[1].to_string(),
        index: fields[2].parse().ok(),
    })
}

// ---------------------------------------------------------------------------
// Phase collect.

struct Collector<'a> {
    args: &'a Args,
    pool: &'a RpcPool,
    work: &'a WorkDir,
    state: Mutex<CollectState>,
    queue: Mutex<(File, HashSet<u128>)>,
    new_signatures: AtomicU64,
    pages: AtomicU64,
}

impl Collector<'_> {
    fn collect_address(&self, address: &str) {
        loop {
            let cursor = self.state.lock().unwrap().addresses[address].clone();
            if cursor.done {
                return;
            }
            let mut options = json!({
                "limit": SIGNATURES_PAGE_LIMIT,
                "commitment": "finalized",
            });
            if let Some(before) = &cursor.before {
                options["before"] = json!(before);
            }
            let page = match self
                .pool
                .call("getSignaturesForAddress", json!([address, options]))
            {
                Ok(Value::Array(page)) => page,
                Ok(other) => die(&format!(
                    "getSignaturesForAddress {address}: unexpected {other}"
                )),
                Err(err) => die(&format!(
                    "getSignaturesForAddress {address}: {err}; re-run to resume"
                )),
            };
            self.pages.fetch_add(1, Ordering::Relaxed);

            let mut reached_start = page.is_empty();
            let mut lines = Vec::new();
            for entry in &page {
                let (Some(slot), Some(signature)) =
                    (entry["slot"].as_u64(), entry["signature"].as_str())
                else {
                    die(&format!(
                        "getSignaturesForAddress {address}: bad entry {entry}"
                    ));
                };
                if slot > self.args.to_slot {
                    continue;
                }
                if slot < self.args.from_slot {
                    reached_start = true;
                    continue;
                }
                let index = entry["transactionIndex"]
                    .as_u64()
                    .map_or_else(|| "-".to_string(), |index| index.to_string());
                let failed = u8::from(!entry["err"].is_null());
                lines.push((
                    signature.to_string(),
                    format!("{slot}\t{signature}\t{index}\t{failed}\t{address}\n"),
                ));
            }

            let mut added = 0;
            {
                let mut queue = self.queue.lock().unwrap();
                let (file, seen) = &mut *queue;
                let mut buffer = String::new();
                for (signature, line) in &lines {
                    if seen.insert(signature_key(signature).unwrap_or_default()) {
                        buffer.push_str(line);
                        added += 1;
                    }
                }
                if let Err(err) = file
                    .write_all(buffer.as_bytes())
                    .and_then(|_| file.sync_data())
                {
                    die(&format!("cannot append to queue.tsv: {err}"));
                }
            }
            self.new_signatures.fetch_add(added, Ordering::Relaxed);

            let mut state = self.state.lock().unwrap();
            let cursor = state.addresses.get_mut(address).unwrap();
            cursor.pages += 1;
            cursor.found += lines.len() as u64;
            cursor.before = page
                .last()
                .and_then(|entry| entry["signature"].as_str())
                .map(str::to_string)
                .or(cursor.before.take());
            cursor.done = reached_start;
            if cursor.done {
                say!(
                    "[collect] {address}: done, {} signatures in range over {} pages",
                    cursor.found,
                    cursor.pages
                );
            }
            self.work.save_state(&state);
        }
    }
}

fn run_collect(args: &Args, pool: &RpcPool, work: &WorkDir, mentions: &[String]) {
    let mut state = work.load_state(args);
    for mention in mentions {
        state.addresses.entry(mention.clone()).or_default();
    }
    work.save_state(&state);

    let mut seen = HashSet::new();
    WorkDir::for_each_line(&work.queue(), |fields| {
        if let Some(entry) = parse_queue_line(fields) {
            seen.insert(signature_key(&entry.signature).unwrap());
        }
    });
    let pending: Vec<String> = mentions
        .iter()
        .filter(|mention| !state.addresses[*mention].done)
        .cloned()
        .collect();
    say!(
        "[collect] slots {}..={}: {} addresses, {} pending, {} signatures already queued",
        args.from_slot,
        args.to_slot,
        mentions.len(),
        pending.len(),
        seen.len()
    );
    let collector = Collector {
        args,
        pool,
        work,
        state: Mutex::new(state),
        queue: Mutex::new((WorkDir::append(&work.queue()), seen)),
        new_signatures: AtomicU64::default(),
        pages: AtomicU64::default(),
    };
    let next = AtomicUsize::new(0);
    let finished = AtomicBool::new(false);
    let threads = worker_count(args, pool).min(pending.len()).max(1);
    thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    while let Some(address) = pending.get(next.fetch_add(1, Ordering::Relaxed)) {
                        collector.collect_address(address);
                    }
                })
            })
            .collect();
        scope.spawn(|| {
            let start = Instant::now();
            let mut last = Instant::now();
            while !finished.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(200));
                if last.elapsed() >= Duration::from_secs(args.progress_secs) {
                    last = Instant::now();
                    let state = collector.state.lock().unwrap();
                    let active: Vec<String> = state
                        .addresses
                        .iter()
                        .filter(|(address, cursor)| {
                            !cursor.done && cursor.pages > 0 && pending.contains(address)
                        })
                        .map(|(address, cursor)| {
                            format!("{}..:{}p/{}", &address[..6], cursor.pages, cursor.found)
                        })
                        .collect();
                    say!(
                        "[collect] {:.0}s pages={} new_signatures={} active=[{}] | {}",
                        start.elapsed().as_secs_f64(),
                        collector.pages.load(Ordering::Relaxed),
                        collector.new_signatures.load(Ordering::Relaxed),
                        active.join(" "),
                        pool.stats()
                    );
                }
            }
        });
        for worker in workers {
            let _ = worker.join();
        }
        finished.store(true, Ordering::Relaxed);
    });
    let queued = collector.queue.lock().unwrap().1.len();
    say!(
        "[collect] finished: {} new signatures this run, {queued} unique signatures queued",
        collector.new_signatures.load(Ordering::Relaxed)
    );
}

// ---------------------------------------------------------------------------
// Phase fetch.

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Outcome {
    /// Row written (or printed with --dry-run).
    Ok,
    /// The transaction does not mention any selected key; the plugin would not
    /// have stored it.
    NotSelected,
    /// The RPC cannot return this transaction version.
    Unsupported,
    /// The RPC reports the transaction in a slot outside the range.
    OutOfRange,
    /// Not finished; retried on the next run (failed.tsv).
    Failed,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotSelected => "not_selected",
            Self::Unsupported => "unsupported_version",
            Self::OutOfRange => "out_of_range",
            Self::Failed => "failed",
        }
    }
}

struct BlockMeta {
    parent: u64,
    info: DbBlockInfo,
}

struct FetchResult {
    signature: String,
    slot: u64,
    outcome: Outcome,
    detail: String,
    row: Option<Box<DbTransaction>>,
    block: Option<BlockMeta>,
}

struct Fetcher<'a> {
    args: &'a Args,
    pool: &'a RpcPool,
    selector: TransactionSelector,
    /// Slots whose block was fetched in this run (--with-blocks).
    block_slots: Mutex<HashSet<u64>>,
}

impl Fetcher<'_> {
    fn fetch_transaction(&self, signature: &str) -> Result<NativeTransaction, (Outcome, String)> {
        let params = json!([signature, {
            "encoding": "base64",
            "commitment": "finalized",
            "maxSupportedTransactionVersion": 1,
        }]);
        let mut nulls = 0;
        let result = loop {
            match self.pool.call("getTransaction", params.clone()) {
                Ok(Value::Null) if nulls < NULL_RESULT_RETRIES => {
                    nulls += 1;
                    thread::sleep(backoff(nulls));
                }
                Ok(Value::Null) => {
                    return Err((Outcome::Failed, "getTransaction returned null".to_string()))
                }
                Ok(result) => break result,
                Err(RpcFailure::Permanent { code, message })
                    if code == UNSUPPORTED_VERSION_ERROR =>
                {
                    return Err((Outcome::Unsupported, message))
                }
                Err(err) => return Err((Outcome::Failed, err.to_string())),
            }
        };
        let response: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_value(result)
            .map_err(|err| {
                (
                    Outcome::Failed,
                    format!("unexpected getTransaction reply: {err}"),
                )
            })?;
        let native = convert_rpc_transaction(response)
            .map_err(|err| (Outcome::Failed, format!("conversion failed: {err}")))?;
        if native.transaction.signatures[0].to_string() != signature {
            return Err((
                Outcome::Failed,
                "reply is for another signature".to_string(),
            ));
        }
        Ok(native)
    }

    fn fetch_block(&self, slot: u64) -> Result<BlockMeta, String> {
        let params = json!([slot, {
            "encoding": "base64",
            "transactionDetails": "none",
            "rewards": false,
            "commitment": "finalized",
            "maxSupportedTransactionVersion": 1,
        }]);
        let block = self
            .pool
            .call("getBlock", params)
            .map_err(|err| format!("getBlock {slot}: {err}"))?;
        let (Some(parent), Some(blockhash)) =
            (block["parentSlot"].as_u64(), block["blockhash"].as_str())
        else {
            return Err(format!("getBlock {slot}: unexpected reply"));
        };
        Ok(BlockMeta {
            parent,
            info: DbBlockInfo {
                slot: slot as i64,
                blockhash: blockhash.to_string(),
                rewards: Vec::new(),
                block_time: block["blockTime"].as_i64(),
                block_height: block["blockHeight"].as_i64(),
            },
        })
    }

    fn process(&self, entry: QueueEntry) -> FetchResult {
        let mut result = FetchResult {
            signature: entry.signature.clone(),
            slot: entry.slot,
            outcome: Outcome::Failed,
            detail: String::new(),
            row: None,
            block: None,
        };
        let mut native = match self.fetch_transaction(&entry.signature) {
            Ok(native) => native,
            Err((outcome, detail)) => {
                result.outcome = outcome;
                result.detail = detail;
                return result;
            }
        };
        result.slot = native.slot;
        if native.slot < self.args.from_slot || native.slot > self.args.to_slot {
            result.outcome = Outcome::OutOfRange;
            result.detail = format!("slot {}", native.slot);
            return result;
        }
        if !self
            .selector
            .is_transaction_selected(native.is_vote, native.mentioned_addresses())
        {
            result.outcome = Outcome::NotSelected;
            return result;
        }
        if native.index.is_none() {
            native.index = entry.index;
        }
        if self.args.with_blocks && self.block_slots.lock().unwrap().insert(native.slot) {
            match self.fetch_block(native.slot) {
                Ok(block) => result.block = Some(block),
                Err(err) => {
                    self.block_slots.lock().unwrap().remove(&native.slot);
                    result.detail = err;
                    return result;
                }
            }
        }
        result.row = Some(Box::new(
            native.to_db_transaction(self.args.store_log_messages),
        ));
        result.outcome = Outcome::Ok;
        result
    }
}

fn worker_count(args: &Args, pool: &RpcPool) -> usize {
    args.workers
        .unwrap_or_else(|| ((pool.total_rps() * 2.0).ceil() as usize).clamp(4, 128))
        .max(1)
}

fn summarize_row(row: &DbTransaction) -> String {
    let (keys, loaded_writable, loaded_readonly) =
        match (&row.legacy_message, &row.v0_loaded_message) {
            (Some(message), _) => (message.account_keys.len(), 0, 0),
            (None, Some(message)) => (
                message.message.account_keys.len(),
                message.loaded_addresses.writable.len(),
                message.loaded_addresses.readonly.len(),
            ),
            (None, None) => (0, 0, 0),
        };
    format!(
        "slot={} index={} sig={} type={} vote={} keys={} loaded={}/{} fee={} error={:?} \
         inner={} token_balances={}/{} rewards={} logs={} write_version={}",
        row.slot,
        row.index,
        bs58::encode(&row.signature).into_string(),
        row.message_type,
        row.is_vote,
        keys,
        loaded_writable,
        loaded_readonly,
        row.meta.fee,
        row.meta.error,
        row.meta.inner_instructions.as_ref().map_or(0, |inner| inner
            .iter()
            .map(|ix| ix.instructions.len())
            .sum()),
        row.meta.pre_token_balances.as_ref().map_or(0, Vec::len),
        row.meta.post_token_balances.as_ref().map_or(0, Vec::len),
        row.meta.rewards.as_ref().map_or(0, Vec::len),
        row.meta
            .log_messages
            .as_ref()
            .map_or_else(|| "NULL".to_string(), |logs| logs.len().to_string()),
        row.write_version,
    )
}

fn bytea(bytes: &[u8]) -> Value {
    let mut text = String::with_capacity(2 + 2 * bytes.len());
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    Value::String(text)
}

fn bytea_array(keys: &[Vec<u8>]) -> Value {
    Value::Array(keys.iter().map(|key| bytea(key)).collect())
}

fn instructions_json(instructions: &[DbCompiledInstruction]) -> Value {
    instructions
        .iter()
        .map(|instruction| {
            json!({
                "program_id_index": instruction.program_id_index,
                "accounts": instruction.accounts,
                "data": bytea(&instruction.data),
            })
        })
        .collect()
}

fn header_json(header: &DbTransactionMessageHeader) -> Value {
    json!({
        "num_required_signatures": header.num_required_signatures,
        "num_readonly_signed_accounts": header.num_readonly_signed_accounts,
        "num_readonly_unsigned_accounts": header.num_readonly_unsigned_accounts,
    })
}

fn token_balances_json(balances: &Option<Vec<DbTransactionTokenBalance>>) -> Value {
    balances.as_ref().map_or(Value::Null, |balances| {
        balances
            .iter()
            .map(|balance| {
                json!({
                    "account_index": balance.account_index,
                    "mint": balance.mint,
                    "ui_token_amount": balance.ui_token_amount,
                    "owner": balance.owner,
                })
            })
            .collect()
    })
}

/// The row as PostgreSQL's `row_to_json` renders it (bytea as "\\x<hex>",
/// composites as objects with the type's field names, enums as labels).
fn row_json(row: &DbTransaction) -> Value {
    let meta = &row.meta;
    json!({
        "signature": bytea(&row.signature),
        "is_vote": row.is_vote,
        "slot": row.slot,
        "message_type": row.message_type,
        "legacy_message": row.legacy_message.as_ref().map(|message| json!({
            "header": header_json(&message.header),
            "account_keys": bytea_array(&message.account_keys),
            "recent_blockhash": bytea(&message.recent_blockhash),
            "instructions": instructions_json(&message.instructions),
        })),
        "v0_loaded_message": row.v0_loaded_message.as_ref().map(|loaded| json!({
            "message": {
                "header": header_json(&loaded.message.header),
                "account_keys": bytea_array(&loaded.message.account_keys),
                "recent_blockhash": bytea(&loaded.message.recent_blockhash),
                "instructions": instructions_json(&loaded.message.instructions),
                "address_table_lookups": loaded.message.address_table_lookups.iter().map(|lookup| json!({
                    "account_key": bytea(&lookup.account_key),
                    "writable_indexes": lookup.writable_indexes,
                    "readonly_indexes": lookup.readonly_indexes,
                })).collect::<Vec<_>>(),
            },
            "loaded_addresses": {
                "writable": bytea_array(&loaded.loaded_addresses.writable),
                "readonly": bytea_array(&loaded.loaded_addresses.readonly),
            },
        })),
        "signatures": bytea_array(&row.signatures),
        "message_hash": bytea(&row.message_hash),
        "meta": {
            "error": meta.error.as_ref().map(|error| json!({
                "error_code": format!("{:?}", error.error_code),
                "error_detail": error.error_detail,
            })),
            "fee": meta.fee,
            "pre_balances": meta.pre_balances,
            "post_balances": meta.post_balances,
            "inner_instructions": meta.inner_instructions.as_ref().map(|inner| inner.iter().map(|inner| json!({
                "index": inner.index,
                "instructions": instructions_json(&inner.instructions),
            })).collect::<Vec<_>>()),
            "log_messages": meta.log_messages,
            "pre_token_balances": token_balances_json(&meta.pre_token_balances),
            "post_token_balances": token_balances_json(&meta.post_token_balances),
            "rewards": meta.rewards.as_ref().map(|rewards| rewards.iter().map(|reward| json!({
                "pubkey": reward.pubkey,
                "lamports": reward.lamports,
                "post_balance": reward.post_balance,
                "reward_type": reward.reward_type.as_ref().map(|reward_type| format!("{reward_type:?}")),
                "commission": reward.commission,
            })).collect::<Vec<_>>()),
        },
        "write_version": row.write_version,
        "index": row.index,
    })
}

struct Database {
    config: GeyserPluginPostgresConfig,
    client: SimplePostgresClient,
    slots_written: HashSet<u64>,
    /// With --with-blocks the slot row is written only together with its block
    /// (which carries the parent): the rooted guard would keep a parent-less
    /// rooted row written first by another transaction of the same slot.
    with_blocks: bool,
}

impl Database {
    fn connect(connection: &str, with_blocks: bool) -> Self {
        let config: GeyserPluginPostgresConfig =
            serde_json::from_value(json!({ "connection_str": connection })).unwrap();
        let mut client = SimplePostgresClient::connect_to_db(&config)
            .unwrap_or_else(|err| die(&format!("cannot connect to the database: {err}")));
        match check_schema(&mut client) {
            Ok(problems) if problems.is_empty() => {}
            Ok(problems) => die(&format!(
                "the database schema is not migrated ({}); run scripts/migrate_agave_v4_3.sql",
                problems.join("; ")
            )),
            Err(err) => die(&format!("cannot check the database schema: {err}")),
        }
        drop(client);
        let client = SimplePostgresClient::new(&config)
            .unwrap_or_else(|err| die(&format!("cannot prepare the statements: {err}")));
        Self {
            config,
            client,
            slots_written: HashSet::new(),
            with_blocks,
        }
    }

    fn write_once(
        &mut self,
        row: &DbTransaction,
        block: Option<&BlockMeta>,
    ) -> Result<(), agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPluginError> {
        let request = LogTransactionRequest {
            transaction_info: row.clone(),
        };
        self.client.log_transaction(request)?;
        let slot = row.slot as u64;
        // A slot is rooted once one of its transactions is finalized; the guard
        // in the statement keeps an existing rooted row as it is.
        let write_slot = if self.with_blocks {
            block.is_some()
        } else {
            !self.slots_written.contains(&slot)
        };
        if write_slot {
            self.client.update_slot_status(
                slot,
                block.map(|block| block.parent),
                SlotStatus::Rooted,
            )?;
            self.slots_written.insert(slot);
        }
        if let Some(block) = block {
            self.client
                .update_block_metadata(UpdateBlockMetadataRequest {
                    block_info: block.info.clone(),
                })?;
        }
        Ok(())
    }

    /// Err when the database rejected the row's values (SQLSTATE 22/23); other
    /// errors are retried with a new connection and abort the run if that fails.
    fn write(&mut self, row: &DbTransaction, block: Option<&BlockMeta>) -> Result<(), String> {
        let mut attempt = 0;
        loop {
            match self.write_once(row, block) {
                Ok(()) => return Ok(()),
                Err(err) if is_row_data_error(&err) => return Err(redact(&err.to_string())),
                Err(err) => {
                    attempt += 1;
                    if attempt > 8 {
                        die(&format!(
                            "database write keeps failing: {err}; progress is saved, re-run to resume"
                        ));
                    }
                    say!("database error (attempt {attempt}): {err}; reconnecting");
                    thread::sleep(backoff(attempt));
                    match SimplePostgresClient::new(&self.config) {
                        Ok(client) => {
                            self.client = client;
                            self.slots_written.clear();
                        }
                        Err(err) => say!("reconnect failed: {err}"),
                    }
                }
            }
        }
    }
}

struct Progress {
    start: Instant,
    pending: u64,
    finished: u64,
    counts: BTreeMap<Outcome, u64>,
    window: VecDeque<(Instant, u64)>,
    last_report: Instant,
}

impl Progress {
    fn record(&mut self, outcome: Outcome) {
        self.finished += 1;
        *self.counts.entry(outcome).or_default() += 1;
    }

    fn report(&mut self, pool: &RpcPool, force: bool, interval: Duration) {
        let now = Instant::now();
        if !force && now.duration_since(self.last_report) < interval {
            return;
        }
        self.last_report = now;
        self.window.push_back((now, self.finished));
        while self.window.len() > 2
            && now.duration_since(self.window[0].0) > Duration::from_secs(120)
        {
            self.window.pop_front();
        }
        let (since, count) = self.window[0];
        let recent =
            (self.finished - count) as f64 / now.duration_since(since).as_secs_f64().max(1e-3);
        let overall = self.finished as f64 / self.start.elapsed().as_secs_f64().max(1e-3);
        let rate = if recent > 0.0 { recent } else { overall };
        let left = self.pending.saturating_sub(self.finished);
        let eta = if rate > 0.0 {
            format_duration(Duration::from_secs_f64(left as f64 / rate))
        } else {
            "?".to_string()
        };
        let counts: Vec<String> = self
            .counts
            .iter()
            .map(|(outcome, count)| format!("{}={count}", outcome.label()))
            .collect();
        say!(
            "[fetch] {}/{} ({:.2}%) {} | {:.1}/s recent {:.1}/s overall | ETA {} | {}",
            self.finished,
            self.pending,
            100.0 * self.finished as f64 / self.pending.max(1) as f64,
            counts.join(" "),
            recent,
            overall,
            eta,
            pool.stats()
        );
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}h{:02}m{:02}s", secs / 3600, secs / 60 % 60, secs % 60)
}

fn run_fetch(args: &Args, pool: &RpcPool, work: &WorkDir, mentions: &[String]) {
    let selector = TransactionSelector::new(mentions)
        .unwrap_or_else(|err| die(&format!("invalid mention: {err}")));
    let mut database = if args.dry_run {
        None
    } else {
        let connection = args
            .db
            .as_deref()
            .unwrap_or_else(|| die("--db (or PG_BACKFILL_DB) is required unless --dry-run"));
        Some(Database::connect(connection, args.with_blocks))
    };

    let mut done = HashSet::new();
    WorkDir::for_each_line(&work.done(), |fields| {
        if let Some(key) = fields
            .first()
            .and_then(|signature| signature_key(signature))
        {
            done.insert(key);
        }
    });
    let mut queued = 0u64;
    let mut pending = 0u64;
    let mut seen = HashSet::new();
    WorkDir::for_each_line(&work.queue(), |fields| {
        if let Some(entry) = parse_queue_line(fields) {
            let key = signature_key(&entry.signature).unwrap();
            if seen.insert(key) {
                queued += 1;
                if !done.contains(&key) {
                    pending += 1;
                }
            }
        }
    });
    drop(seen);
    if queued == 0 {
        die(&format!(
            "{} is empty; run --phase collect first",
            work.queue().display()
        ));
    }
    let pending = args.limit.map_or(pending, |limit| pending.min(limit));
    let workers = worker_count(args, pool);
    say!(
        "[fetch] {queued} queued, {} done earlier, {pending} to fetch with {workers} workers \
         ({:.1} req/s over {} endpoint(s)){}{}",
        done.len(),
        pool.total_rps(),
        pool.endpoints.len(),
        if args.with_blocks {
            ", with blocks"
        } else {
            ""
        },
        if args.dry_run { ", DRY RUN" } else { "" },
    );

    let fetcher = Fetcher {
        args,
        pool,
        selector,
        block_slots: Mutex::new(HashSet::new()),
    };
    let (work_sender, work_receiver): (Sender<QueueEntry>, Receiver<QueueEntry>) =
        bounded(workers * 4);
    let (result_sender, result_receiver) = bounded::<FetchResult>(workers * 4);
    let mut done_file = (!args.dry_run).then(|| WorkDir::append(&work.done()));
    let mut failed_file = (!args.dry_run).then(|| WorkDir::append(&work.failed()));
    let mut dump_file = match (&args.dump_rows, args.dry_run) {
        (Some(path), true) => Some(
            File::create(path)
                .unwrap_or_else(|err| die(&format!("cannot create {}: {err}", path.display()))),
        ),
        (Some(_), false) => die("--dump-rows needs --dry-run"),
        (None, _) => None,
    };
    let mut progress = Progress {
        start: Instant::now(),
        pending,
        finished: 0,
        counts: BTreeMap::new(),
        window: VecDeque::new(),
        last_report: Instant::now(),
    };
    let interval = Duration::from_secs(args.progress_secs);

    thread::scope(|scope| {
        let queue_path = work.queue();
        let done = &done;
        scope.spawn(move || {
            let mut sent = 0u64;
            let mut seen = HashSet::new();
            let Ok(file) = File::open(&queue_path) else {
                return;
            };
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                let fields: Vec<&str> = line.split('\t').collect();
                let Some(entry) = parse_queue_line(&fields) else {
                    continue;
                };
                let key = signature_key(&entry.signature).unwrap();
                if done.contains(&key) || !seen.insert(key) {
                    continue;
                }
                if sent >= pending || work_sender.send(entry).is_err() {
                    break;
                }
                sent += 1;
            }
        });
        for _ in 0..workers {
            let work_receiver = work_receiver.clone();
            let result_sender = result_sender.clone();
            let fetcher = &fetcher;
            scope.spawn(move || {
                for entry in work_receiver {
                    if result_sender.send(fetcher.process(entry)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(result_sender);

        let mut printed_full = false;
        loop {
            let result = match result_receiver.recv_timeout(interval) {
                Ok(result) => result,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    progress.report(pool, false, interval);
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            let mut outcome = result.outcome;
            let mut detail = result.detail;
            if let Some(row) = &result.row {
                if let Some(database) = database.as_mut() {
                    if let Err(err) = database.write(row, result.block.as_ref()) {
                        outcome = Outcome::Failed;
                        detail = format!("database rejected the row: {err}");
                    }
                } else {
                    say!("[dry-run] {}", summarize_row(row));
                    if let Some(block) = &result.block {
                        say!(
                            "[dry-run]   block slot={} parent={} blockhash={} time={:?} height={:?}",
                            block.info.slot,
                            block.parent,
                            block.info.blockhash,
                            block.info.block_time,
                            block.info.block_height
                        );
                    }
                    if let Some(file) = dump_file.as_mut() {
                        if let Err(err) = writeln!(file, "{}", row_json(row)) {
                            die(&format!("cannot write --dump-rows: {err}"));
                        }
                    }
                    if args.verbose || !printed_full {
                        say!("[dry-run]   full row: {row:?}");
                        printed_full = true;
                    }
                }
            }
            if outcome == Outcome::Failed {
                say!(
                    "[fetch] {} slot {}: {detail}",
                    result.signature,
                    result.slot
                );
                if let Some(file) = failed_file.as_mut() {
                    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
                    let _ = writeln!(
                        file,
                        "{}\t{}\t{now}\t{detail}",
                        result.signature, result.slot
                    );
                }
            } else {
                if outcome != Outcome::Ok {
                    say!(
                        "[fetch] {} slot {}: {} {detail}",
                        result.signature,
                        result.slot,
                        outcome.label()
                    );
                }
                if let Some(file) = done_file.as_mut() {
                    if let Err(err) = writeln!(
                        file,
                        "{}\t{}\t{}",
                        result.signature,
                        outcome.label(),
                        result.slot
                    ) {
                        die(&format!("cannot append to done.tsv: {err}"));
                    }
                }
            }
            progress.record(outcome);
            progress.report(pool, false, interval);
        }
    });
    progress.report(pool, true, interval);
    let failed = progress
        .counts
        .get(&Outcome::Failed)
        .copied()
        .unwrap_or_default();
    say!(
        "[fetch] finished in {}{}",
        format_duration(progress.start.elapsed()),
        if failed > 0 {
            format!(
                "; {failed} signatures failed (see {}), re-run to retry them",
                work.failed().display()
            )
        } else {
            String::new()
        }
    );
}

fn main() {
    let args = parse_args();
    if let Err(err) = fs::create_dir_all(&args.work_dir) {
        die(&format!("cannot create {}: {err}", args.work_dir.display()));
    }
    // Held until exit; a second run on the same work dir would interleave the
    // work files.
    let lock = WorkDir::append(&args.work_dir.join("lock"));
    if lock.try_lock().is_err() {
        die(&format!(
            "another pg_backfill is running on {}",
            args.work_dir.display()
        ));
    }
    let _ = LOG_FILE.set(Mutex::new(WorkDir::append(
        &args.work_dir.join("backfill.log"),
    )));
    let mut redactions: Vec<(String, String)> = args
        .rpcs
        .iter()
        .map(|(url, _)| (url.clone(), display_url(url)))
        .collect();
    if let Some(password) = args.db.as_deref().and_then(connection_password) {
        redactions.push((password, "<password>".to_string()));
    }
    let _ = REDACTIONS.set(redactions);
    let _ = log::set_logger(&Logger);
    log::set_max_level(log::LevelFilter::Warn);

    let work = WorkDir {
        root: args.work_dir.clone(),
    };
    let mentions = read_mentions(
        args.mentions_file
            .as_deref()
            .unwrap_or_else(|| die("--mentions-file is required")),
    );
    if let Err(err) = TransactionSelector::new(&mentions) {
        die(&format!("invalid mention: {err}"));
    }
    let pool = RpcPool::new(&args.rpcs, args.max_retries);
    say!(
        "pg_backfill: slots {}..={} phase={} endpoints=[{}] store_log_messages={}",
        args.from_slot,
        args.to_slot,
        match args.phase {
            Phase::Collect => "collect",
            Phase::Fetch => "fetch",
            Phase::All => "all",
        },
        pool.endpoints
            .iter()
            .map(|endpoint| format!(
                "{} @{:.1}/s",
                endpoint.display,
                1.0 / endpoint.interval.as_secs_f64()
            ))
            .collect::<Vec<_>>()
            .join(", "),
        args.store_log_messages
    );
    if matches!(args.phase, Phase::Collect | Phase::All) {
        run_collect(&args, &pool, &work, &mentions);
    }
    if matches!(args.phase, Phase::Fetch | Phase::All) {
        run_fetch(&args, &pool, &work, &mentions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_password() {
        assert_eq!(
            connection_password("host=127.0.0.1 user=solana password=s3cret dbname=solana"),
            Some("s3cret".to_string())
        );
        assert_eq!(
            connection_password("postgres://solana:s3cret@127.0.0.1/solana"),
            Some("s3cret".to_string())
        );
        assert_eq!(connection_password("host=127.0.0.1 user=solana"), None);
        assert_eq!(
            connection_password("postgres://solana@127.0.0.1/solana"),
            None
        );
    }

    #[test]
    fn test_display_url_hides_keys() {
        assert_eq!(
            display_url("https://rpc.example.com/v1/KEY?api-key=KEY"),
            "https://rpc.example.com"
        );
        assert_eq!(
            display_url("https://user:pw@host:8899/"),
            "https://host:8899"
        );
    }

    /// Values equal as PostgreSQL's `row_to_json` prints them: numbers compare
    /// as f64 (a stored 0.0 prints as `0`).
    fn assert_same_json(stored: &Value, built: &Value, path: &str) {
        match (stored, built) {
            (Value::Object(stored), Value::Object(built)) => {
                let keys: HashSet<&String> = stored.keys().chain(built.keys()).collect();
                for key in keys {
                    assert_same_json(
                        stored.get(key).unwrap_or(&Value::Null),
                        built.get(key).unwrap_or(&Value::Null),
                        &format!("{path}.{key}"),
                    );
                }
            }
            (Value::Array(stored), Value::Array(built)) => {
                assert_eq!(stored.len(), built.len(), "{path}");
                for (index, (stored, built)) in stored.iter().zip(built).enumerate() {
                    assert_same_json(stored, built, &format!("{path}[{index}]"));
                }
            }
            (Value::Number(stored), Value::Number(built)) => {
                assert_eq!(stored.as_f64(), built.as_f64(), "{path}")
            }
            _ => assert_eq!(stored, built, "{path}"),
        }
    }

    fn built_row(rpc: &str) -> Value {
        let response = serde_json::from_str(rpc).unwrap();
        row_json(
            &convert_rpc_transaction(response)
                .unwrap()
                .to_db_transaction(false),
        )
    }

    /// A row the live plugin (agave v4.3.0) wrote on mainnet-la, exported with
    /// `row_to_json`, against the row built from getTransaction for the same
    /// signature. Only write_version (a per-process counter) differs.
    #[test]
    fn test_matches_row_stored_by_live_plugin() {
        let stored: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/backfill/live_v1.db_row.json"
        ))
        .unwrap();
        let mut built = built_row(include_str!(
            "../../tests/fixtures/backfill/live_v1.rpc.json"
        ));
        built["write_version"] = stored["write_version"].clone();
        assert_same_json(&stored, &built, "");
    }

    /// A v0 row with zero token balances written by the plugin before the gap
    /// (exported columns: signature, slot, message_type, index, meta). The
    /// stored ui_token_amount 0 is reproduced although getTransaction returns
    /// `uiAmount: null` for it.
    #[test]
    fn test_matches_stored_zero_token_amounts() {
        let stored: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/backfill/live_v0_zero_amounts.db_row.json"
        ))
        .unwrap();
        let built = built_row(include_str!(
            "../../tests/fixtures/backfill/live_v0_zero_amounts.rpc.json"
        ));
        for key in ["signature", "slot", "message_type", "index", "meta"] {
            assert_same_json(&stored[key], &built[key], key);
        }
        let zeros = stored["meta"]["post_token_balances"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|balance| balance["ui_token_amount"].as_f64() == Some(0.0))
            .count();
        assert!(zeros > 0);
    }

    #[test]
    fn test_parse_queue_line() {
        let signature = bs58::encode([7u8; 64]).into_string();
        let line = format!("100\t{signature}\t5\t0\taddr");
        let fields: Vec<&str> = line.split('\t').collect();
        let entry = parse_queue_line(&fields).unwrap();
        assert_eq!((entry.slot, entry.index), (100, Some(5)));
        let line = format!("100\t{signature}\t-\t1\taddr");
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(parse_queue_line(&fields).unwrap().index, None);
        // A torn line is skipped.
        assert!(parse_queue_line(&["100", &signature[..20]]).is_none());
    }
}
