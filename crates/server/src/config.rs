//! One configuration mechanism: CLI flags, each backed by an environment
//! variable, each with a default.
//!
//! Secrets are never flags -- they come from the environment only, and are
//! read by the adapter that needs them.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use app_core::WebConfig;
use app_core::market::Region;
use clap::Parser;
use cluster_core::{HealthPolicy, Role, RolePolicies, RolePolicy};
use cluster_local::LocalClusterConfig;

#[derive(Debug, Parser)]
#[command(
    name = "wow-auction-tracker",
    about = "Auction tracker with a built-in work cluster"
)]
pub struct Cli {
    /// Address to bind the HTTP server to.
    #[arg(long, env = "APP_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// Port to bind the HTTP server to.
    #[arg(long, env = "APP_PORT", default_value_t = 3000)]
    pub port: u16,

    /// SQLite database file. Use `:memory:` for a throwaway run.
    #[arg(long, env = "APP_DATABASE", default_value = "data/cluster.db")]
    pub database: PathBuf,

    /// Workers to run inside this process. Enough for a single server: they
    /// are Tokio tasks and cost nothing when idle.
    #[arg(long, env = "APP_WORKERS", default_value_t = Self::DEFAULT_WORKERS)]
    pub workers: u16,

    /// Also accept workers that connect over the network, so the cluster can
    /// outgrow one machine. Off by default -- a single server needs only the
    /// in-process pool.
    ///
    /// Workers dial this address; nothing here dials a worker. Bind
    /// `0.0.0.0:3001` rather than `127.0.0.1:3001` or nothing off-box can
    /// reach it.
    #[arg(long, env = "APP_WORKER_LISTEN")]
    pub worker_listen: Option<SocketAddr>,

    #[arg(long, env = "APP_MAX_WORKER_CONNECTIONS", default_value_t = 64)]
    pub max_worker_connections: usize,

    #[arg(long, env = "APP_MAX_WORKER_HANDSHAKES", default_value_t = 16)]
    pub max_worker_handshakes: usize,

    /// Run as a worker instead of a web server: connect to the coordinator at
    /// this address, take work, and exit when it goes away.
    ///
    /// The same binary either way, which is what keeps a worker's build
    /// identical to the one that was tested.
    #[arg(long, env = "APP_CONNECT", conflicts_with_all = ["worker_listen", "workers"])]
    pub connect: Option<String>,

    /// How often each node emits a heartbeat.
    #[arg(long, env = "APP_HEARTBEAT_MS", default_value_t = 1_000)]
    pub heartbeat_ms: u64,

    /// Heartbeat silence after which a node becomes Suspect.
    #[arg(long, env = "APP_SUSPECT_MS", default_value_t = 3_000)]
    pub suspect_ms: u64,

    /// Heartbeat silence after which a node is declared Offline and its tasks
    /// are requeued.
    #[arg(long, env = "APP_OFFLINE_MS", default_value_t = 6_000)]
    pub offline_ms: u64,

    /// Total attempts per task before it fails for good.
    #[arg(long, env = "APP_MAX_ATTEMPTS", default_value_t = 3)]
    pub max_task_attempts: u16,

    /// Fallback refresh interval for the live fragments. Updates normally
    /// arrive over SSE; this is the safety net when the stream is unavailable.
    #[arg(long, env = "APP_POLL_MS", default_value_t = 2_000)]
    pub poll_ms: u64,

    #[arg(long, env = "APP_GATEWAY_MIN", default_value_t = 1)]
    pub gateway_min: usize,
    #[arg(long, env = "APP_FRONTEND_MIN", default_value_t = 2)]
    pub frontend_min: usize,
    #[arg(long, env = "APP_BACKEND_MIN", default_value_t = 2)]
    pub backend_min: usize,
    #[arg(long, env = "APP_STORAGE_MIN", default_value_t = 1)]
    pub storage_min: usize,
    #[arg(long, env = "APP_COORDINATOR_MIN", default_value_t = 1)]
    pub coordinator_min: usize,

    /// Expose the failure-simulation routes: stop a node, drop its
    /// heartbeats, make its next task fail.
    ///
    /// **Off by default.** They are behind the administrator gate, but a
    /// control that can take a node down is not something a deployment should
    /// have to remember to remove -- it is something a deployment should have
    /// to ask for. Turn it on to demonstrate a requeue.
    #[arg(long, env = "APP_DEBUG_CONTROLS", default_value_t = false, action = clap::ArgAction::Set)]
    pub debug_controls: bool,

    /// Shared secret a connecting worker must present, and that a worker
    /// started with `--connect` sends.
    ///
    /// Environment only, never a flag: a secret on a command line is in every
    /// `ps` listing and every shell history (CLAUDE.md §10). Required whenever
    /// `--worker-listen` is set -- an open worker socket without one admits
    /// anybody who can reach the port, and five bytes was the whole handshake.
    #[arg(skip)]
    pub cluster_token: Option<String>,

    /// One-shot administrator bootstrap credentials. Environment-only so the
    /// password never appears in `ps` or shell history. Both must be present.
    #[arg(skip)]
    pub bootstrap_admin: Option<(String, String)>,

    /// Break every response's time down in a `Server-Timing` header: database,
    /// cache, analysis and template time, plus the statement and row counts.
    ///
    /// **Off by default.** Those numbers describe how the deployment is doing,
    /// which CLAUDE.md §7 keeps on the operations side; a visitor is owed the
    /// page, not the shape of the read path behind it. Turn it on to measure:
    /// `scripts/bench.py` does exactly that, and the browser's network panel
    /// reads the header without any further tooling.
    #[arg(long, env = "APP_SERVER_TIMING", default_value_t = false, action = clap::ArgAction::Set)]
    pub server_timing: bool,

    /// Render TSM-derived values in public pages. Collection and the internal
    /// contrast test still run while this is off.
    #[arg(long, env = "APP_SHOW_TSM_DATA", default_value_t = false, action = clap::ArgAction::Set)]
    pub show_tsm_data: bool,

    /// Mark cookies `Secure`. Requires HTTPS; off for local plain-HTTP dev.
    #[arg(long, env = "APP_SECURE_COOKIES", default_value_t = false, action = clap::ArgAction::Set)]
    pub secure_cookies: bool,

    /// Trust `Forwarded`/`X-Forwarded-For` for per-origin limits. Enable only
    /// when the service is reachable exclusively through a proxy that
    /// overwrites these headers.
    #[arg(long, env = "APP_TRUST_PROXY_HEADERS", default_value_t = false, action = clap::ArgAction::Set)]
    pub trust_proxy_headers: bool,

    /// How long upstream WoW responses stay cached, in seconds.
    #[arg(long, env = "APP_CACHE_TTL_SECS", default_value_t = 600)]
    pub cache_ttl_secs: u64,

    /// Regions to collect auction-house prices for, comma separated.
    /// Commodity markets are region-wide and entirely separate from each other.
    #[arg(
        long,
        env = "APP_MARKET_REGIONS",
        default_value = "eu,us,kr,tw",
        value_delimiter = ','
    )]
    pub market_regions: Vec<String>,

    /// Connected realms to collect gear prices from, as `region:id`, comma
    /// separated -- for example `eu:1403,us:60`.
    ///
    /// Empty, the default, means **every** connected realm in every collected
    /// region: 184 of them across EU, US, KR and TW. Gear is not a commodity,
    /// so each realm is its own ~20 MB fetch; they run as many at a time as
    /// the cluster has nodes. Which realms are actually collected can then be
    /// changed at runtime from the admin page -- this flag only seeds it.
    #[arg(
        long,
        env = "APP_MARKET_REALMS",
        default_value = "",
        value_delimiter = ','
    )]
    pub market_realms: Vec<String>,

    /// How often to poll the commodities endpoint, in minutes. Upstream only
    /// changes hourly, so more often than that mostly yields 304s.
    #[arg(long, env = "APP_MARKET_INTERVAL_MIN", default_value_t = 30)]
    pub market_interval_minutes: u64,

    /// How long price history is kept, in days. Zero keeps it forever, which
    /// is what the archive is for -- growth is handled by downsampling.
    #[arg(long, env = "APP_MARKET_RETAIN_DAYS", default_value_t = 0)]
    pub market_retain_days: u64,

    /// How long samples stay at full resolution, in days. Older days are
    /// collapsed to one row each: the archive survives, its resolution does
    /// not. Zero disables it.
    #[arg(long, env = "APP_MARKET_DOWNSAMPLE_DAYS", default_value_t = 14)]
    pub market_downsample_days: u64,

    /// How long price ladders are kept, in days -- the depth "hot window".
    ///
    /// Their own policy because they are bulky: a ladder is every rung of a
    /// market's supply, not five summary numbers. Zero keeps them forever,
    /// which will need a disk. The compact historical encoding is not built
    /// yet on purpose (CLAUDE.md §16, Phase 7): choosing it before there are
    /// real ladders to prove which analyses survive it would be picking an
    /// archive format blind.
    #[arg(long, env = "APP_MARKET_LADDER_DAYS", default_value_t = 14)]
    pub market_ladder_days: u64,

    /// Terminal jobs and cluster events retained for diagnosis. Zero keeps
    /// operational history forever.
    #[arg(long, env = "APP_OPERATION_RETENTION_DAYS", default_value_t = 90)]
    pub operation_retention_days: u64,

    /// Tracing filter, e.g. `info`, `debug`, `server=debug,cluster_local=trace`.
    #[arg(
        long,
        env = "APP_LOG",
        default_value = "info,sqlx=warn,tower_http=info"
    )]
    pub log: String,
}

/// Where the cluster join token comes from.
pub const CLUSTER_TOKEN_ENV: &str = "APP_CLUSTER_TOKEN";

impl Cli {
    /// Read the join token out of the environment and check it is present
    /// where it is needed.
    ///
    /// A worker socket with no token is not a smaller version of a secured
    /// one, it is an open door: five bytes on that port used to be the whole
    /// of joining the cluster, taking work and reporting whatever outcome you
    /// liked. So refusing to start beats starting and mentioning it in a log
    /// nobody reads.
    ///
    /// Nothing is required of the default single-process deployment, whose
    /// workers are Tokio tasks that never touch a socket.
    pub fn resolve_cluster_token(&mut self) -> anyhow::Result<()> {
        self.cluster_token = std::env::var(CLUSTER_TOKEN_ENV)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());

        if self.worker_listen.is_some() && self.cluster_token.is_none() {
            anyhow::bail!(
                "--worker-listen is set but {CLUSTER_TOKEN_ENV} is not. \
                 Any process that can reach that port would join the cluster, \
                 take work and report results for it. Set {CLUSTER_TOKEN_ENV} \
                 to a long random string, the same value on the coordinator \
                 and on every worker."
            );
        }
        Ok(())
    }

    pub fn resolve_bootstrap_admin(&mut self) -> anyhow::Result<()> {
        let username = secret_env("APP_BOOTSTRAP_ADMIN_USERNAME");
        let password = secret_env("APP_BOOTSTRAP_ADMIN_PASSWORD");
        self.bootstrap_admin = match (username, password) {
            (Some(username), Some(password)) => Some((username, password)),
            (None, None) => None,
            _ => anyhow::bail!(
                "APP_BOOTSTRAP_ADMIN_USERNAME and APP_BOOTSTRAP_ADMIN_PASSWORD must be set together"
            ),
        };
        Ok(())
    }

    /// The `--workers` default.
    const DEFAULT_WORKERS: u16 = 4;

    pub fn role_policies(&self) -> RolePolicies {
        let mut policies = RolePolicies::default();
        policies.set(Role::Gateway, RolePolicy::new(self.gateway_min));
        policies.set(Role::Frontend, RolePolicy::new(self.frontend_min));
        policies.set(Role::Backend, RolePolicy::new(self.backend_min));
        policies.set(Role::Storage, RolePolicy::new(self.storage_min));
        policies.set(Role::Coordinator, RolePolicy::new(self.coordinator_min));
        policies
    }

    pub fn cluster_config(&self) -> LocalClusterConfig {
        LocalClusterConfig {
            node_count: self.workers,
            // Workers normally arrive anonymously and are given an id, so
            // nothing has to be declared up front.
            remote_nodes: Vec::new(),
            node_listen: self.worker_listen,
            join_token: self.cluster_token.clone(),
            max_remote_connections: self.max_worker_connections.max(1),
            max_pending_handshakes: self
                .max_worker_handshakes
                .clamp(1, self.max_worker_connections.max(1)),
            health: HealthPolicy {
                heartbeat_interval_ms: self.heartbeat_ms,
                suspect_after_ms: self.suspect_ms,
                offline_after_ms: self.offline_ms,
            },
            policies: self.role_policies(),
            max_task_attempts: self.max_task_attempts,
            ..LocalClusterConfig::default()
        }
    }

    pub fn web_config(&self) -> WebConfig {
        WebConfig {
            poll_interval_ms: self.poll_ms,
            debug_controls: self.debug_controls,
            server_timing: self.server_timing,
            show_tsm_data: self.show_tsm_data,
            secure_cookies: self.secure_cookies,
            trust_proxy_headers: self.trust_proxy_headers,
            upstream_cache_ttl_ms: self.cache_ttl_secs * 1_000,
            ..WebConfig::default()
        }
    }

    /// Parsed collection regions, ignoring anything unrecognised.
    pub fn regions(&self) -> Vec<Region> {
        let mut regions: Vec<Region> = self
            .market_regions
            .iter()
            .filter_map(|r| {
                let parsed = Region::parse(r.trim());
                if parsed.is_none() && !r.trim().is_empty() {
                    tracing::warn!(region = %r, "ignoring unknown market region");
                }
                parsed
            })
            .collect();
        regions.sort();
        regions.dedup();
        regions
    }

    /// Parsed gear realms, ignoring anything unrecognised.
    ///
    /// A malformed entry is a warning rather than a failure to start: one
    /// mistyped realm must not take the whole tracker down with it.
    pub fn realms(&self) -> Vec<(Region, app_core::market::RealmId)> {
        let mut realms: Vec<(Region, app_core::market::RealmId)> = self
            .market_realms
            .iter()
            .filter_map(|entry| {
                let entry = entry.trim();
                if entry.is_empty() {
                    return None;
                }
                let parsed = entry
                    .split_once(':')
                    .and_then(|(region, id)| {
                        Some((Region::parse(region.trim())?, id.trim().parse().ok()?))
                    })
                    .map(|(region, id)| (region, app_core::market::RealmId(id)));
                if parsed.is_none() {
                    tracing::warn!(realm = %entry, "ignoring unparseable market realm, expected region:id");
                }
                parsed
            })
            .collect();
        realms.sort();
        realms.dedup();
        realms
    }

    /// The settings worth writing down, as JSON. Recorded at boot through the
    /// key/value port so a misbehaving run can be explained afterwards.
    pub fn effective_settings_json(&self) -> String {
        format!(
            concat!(
                r#"{{"workers":{},"heartbeat_ms":{},"suspect_ms":{},"offline_ms":{},"#,
                r#""max_task_attempts":{},"gateway_min":{},"frontend_min":{},"#,
                r#""backend_min":{},"storage_min":{},"coordinator_min":{},"#,
                r#""debug_controls":{},"server_timing":{}}}"#
            ),
            self.workers,
            self.heartbeat_ms,
            self.suspect_ms,
            self.offline_ms,
            self.max_task_attempts,
            self.gateway_min,
            self.frontend_min,
            self.backend_min,
            self.storage_min,
            self.coordinator_min,
            self.debug_controls,
            self.server_timing,
        )
    }
}

fn secret_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
