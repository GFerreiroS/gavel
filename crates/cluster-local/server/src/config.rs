//! One configuration mechanism: CLI flags, each backed by an environment
//! variable, each with a default (CLAUDE.md 28).
//!
//! Secrets are never flags -- they come from the environment only, and are
//! read by the adapter that needs them.

use std::net::IpAddr;
use std::path::PathBuf;

use app_core::WebConfig;
use clap::Parser;
use cluster_core::{HealthPolicy, Role, RolePolicies, RolePolicy};
use cluster_local::LocalClusterConfig;

#[derive(Debug, Parser)]
#[command(
    name = "esp-web-cluster",
    about = "V0 of the ESP32 web cluster, running on a PC"
)]
pub struct Cli {
    /// Address to bind the HTTP server to.
    #[arg(long, env = "ESP_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// Port to bind the HTTP server to.
    #[arg(long, env = "ESP_PORT", default_value_t = 3000)]
    pub port: u16,

    /// SQLite database file. Use `:memory:` for a throwaway run.
    #[arg(long, env = "ESP_DATABASE", default_value = "data/cluster.db")]
    pub database: PathBuf,

    /// Number of simulated nodes to start.
    #[arg(long, env = "ESP_NODES", default_value_t = 8)]
    pub nodes: u16,

    /// How often each node emits a heartbeat.
    #[arg(long, env = "ESP_HEARTBEAT_MS", default_value_t = 1_000)]
    pub heartbeat_ms: u64,

    /// Heartbeat silence after which a node becomes Suspect.
    #[arg(long, env = "ESP_SUSPECT_MS", default_value_t = 3_000)]
    pub suspect_ms: u64,

    /// Heartbeat silence after which a node is declared Offline and its tasks
    /// are requeued.
    #[arg(long, env = "ESP_OFFLINE_MS", default_value_t = 6_000)]
    pub offline_ms: u64,

    /// Total attempts per task before it fails for good.
    #[arg(long, env = "ESP_MAX_ATTEMPTS", default_value_t = 3)]
    pub max_task_attempts: u16,

    /// How often the browser re-polls the live fragments.
    #[arg(long, env = "ESP_POLL_MS", default_value_t = 2_000)]
    pub poll_ms: u64,

    #[arg(long, env = "ESP_GATEWAY_MIN", default_value_t = 1)]
    pub gateway_min: usize,
    #[arg(long, env = "ESP_FRONTEND_MIN", default_value_t = 2)]
    pub frontend_min: usize,
    #[arg(long, env = "ESP_BACKEND_MIN", default_value_t = 2)]
    pub backend_min: usize,
    #[arg(long, env = "ESP_STORAGE_MIN", default_value_t = 1)]
    pub storage_min: usize,
    #[arg(long, env = "ESP_COORDINATOR_MIN", default_value_t = 1)]
    pub coordinator_min: usize,

    /// Expose the failure-simulation routes. On by default in V0; turn it off
    /// for anything resembling a real deployment.
    #[arg(long, env = "ESP_DEBUG_CONTROLS", default_value_t = true, action = clap::ArgAction::Set)]
    pub debug_controls: bool,

    /// Mark cookies `Secure`. Requires HTTPS; off for local plain-HTTP dev.
    #[arg(long, env = "ESP_SECURE_COOKIES", default_value_t = false, action = clap::ArgAction::Set)]
    pub secure_cookies: bool,

    /// How long upstream WoW responses stay cached, in seconds.
    #[arg(long, env = "ESP_CACHE_TTL_SECS", default_value_t = 600)]
    pub cache_ttl_secs: u64,

    /// Tracing filter, e.g. `info`, `debug`, `server=debug,cluster_local=trace`.
    #[arg(
        long,
        env = "ESP_LOG",
        default_value = "info,sqlx=warn,tower_http=info"
    )]
    pub log: String,
}

impl Cli {
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
            node_count: self.nodes,
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
            secure_cookies: self.secure_cookies,
            upstream_cache_ttl_ms: self.cache_ttl_secs * 1_000,
            ..WebConfig::default()
        }
    }
}
