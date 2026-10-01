use anyhow::{Context, Result, ensure};
use clap::Args;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

const DOTENV_KEYS: [&str; 2] = ["ANTIGRAVITY_MODEL", "ANTIGRAVITY_CREDENTIALS"];

pub fn load_dotenv() -> Result<()> {
    let path = std::path::Path::new(".env");
    if !path.exists() {
        return Ok(());
    }
    for entry in dotenvy::from_path_iter(path).context("parse .env")? {
        let (key, value) = entry.context("invalid .env entry")?;
        if DOTENV_KEYS.contains(&key.as_str()) && std::env::var_os(&key).is_none() {
            // Called before the Tokio runtime starts, while this process is single-threaded.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

#[derive(Args, Clone)]
pub struct Config {
    #[arg(long, env = "ANTIGRAVITY_LISTEN", default_value = "127.0.0.1:8787")]
    pub listen: SocketAddr,
    #[arg(
        long,
        env = "ANTIGRAVITY_BASE_URL",
        default_value = "https://daily-cloudcode-pa.googleapis.com"
    )]
    pub base_url: String,
    #[arg(
        long,
        env = "ANTIGRAVITY_MODEL",
        default_value = "gemini-3.8-flash-medium"
    )]
    pub model: Option<String>,
    #[arg(long, env = "ANTIGRAVITY_PROJECT")]
    pub project: Option<String>,
    #[arg(
        long,
        env = "ANTIGRAVITY_CREDENTIALS",
        default_value = "antigravity.credentials.json"
    )]
    pub credentials: PathBuf,
    #[arg(long, env = "ANTIGRAVITY_ACCESS_TOKEN", hide_env_values = true)]
    pub access_token: Option<String>,
    #[arg(
        long,
        env = "ANTIGRAVITY_TOKEN_URL",
        default_value = "https://oauth2.googleapis.com/token"
    )]
    pub token_url: String,
    #[arg(long, env = "ANTIGRAVITY_TIMEOUT_SECONDS", default_value_t = 120)]
    pub timeout_seconds: u64,
    /// Replay state expires after this many seconds without use.
    #[arg(long, env = "ANTIGRAVITY_STATE_TTL_SECONDS", default_value_t = 86400)]
    pub state_ttl_seconds: u64,
    /// SQLite page cache budget in bytes; does not limit replay data on disk.
    #[arg(long, env = "ANTIGRAVITY_STATE_BYTES", default_value_t = 67108864)]
    pub state_bytes: usize,
    #[arg(long, env = "ANTIGRAVITY_SCHEMA_POLICY", default_value = "compatible", value_parser = ["compatible", "reject-lossy"])]
    pub schema_policy: String,
    #[arg(long, env = "ANTIGRAVITY_REQUEST_BYTES", default_value_t = 16777216)]
    pub request_bytes: usize,
    #[arg(long, env = "ANTIGRAVITY_RESPONSE_BYTES", default_value_t = 16777216)]
    pub response_bytes: usize,
    #[arg(long, env = "ANTIGRAVITY_USER_AGENT", default_value = "antigravity")]
    pub user_agent: String,
    #[arg(long, env = "ANTIGRAVITY_REASONING", default_value = "raw-thought", value_parser = ["raw-thought", "hidden"])]
    pub reasoning: String,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.listen.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST),
            "only 127.0.0.1 is supported"
        );
        let url = reqwest::Url::parse(&self.base_url).context("invalid backend URL")?;
        ensure!(
            url.scheme() == "https" || url.host_str() == Some("127.0.0.1"),
            "backend requires HTTPS (HTTP allowed for loopback tests)"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "backend URL must not contain credentials"
        );
        ensure!(
            self.timeout_seconds > 0 && self.state_ttl_seconds > 0,
            "timeouts must be positive"
        );
        ensure!(
            self.state_bytes > 0 && self.request_bytes > 0 && self.response_bytes > 0,
            "byte limits must be positive"
        );
        Ok(())
    }

    pub fn client(&self) -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(self.timeout_seconds))
            .read_timeout(Duration::from_secs(self.timeout_seconds))
            .build()?)
    }
}
