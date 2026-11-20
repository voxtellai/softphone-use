//! Configuration from the environment, after loading `.env.local` and then `.env` from the
//! working directory. Variables already set win over both files.
//! The SIP password is never logged.

use std::path::PathBuf;
use std::time::Duration;

use crate::audio::Mode;

#[derive(Clone, Debug)]
pub struct Config {
    /// Device the softphone registers as, e.g. `1001` on domain `pbx.example.com`.
    pub user: String,
    pub password: String,
    pub domain: String,
    /// Registrar or SBC, e.g. `sbc.example.com:5060`. Defaults to the domain.
    pub proxy: String,
    /// `tcp` or `udp`.
    pub transport: String,
    /// Local control API. MCP and the e2e runner talk to it.
    pub http_addr: String,
    pub data_dir: PathBuf,
    pub xai_key: String,
    pub xai_model: String,
    pub xai_voice: String,
    /// `None` answers only through the API or the UI.
    pub auto_answer: Option<Mode>,
    pub auto_answer_after: Duration,
    pub ring_timeout: Duration,
}

pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:7071";

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        load_env_files();
        let auto = env_or("SOFTPHONE_AUTO_ANSWER", "manual");
        let cfg = Config {
            user: env_or("SOFTPHONE_SIP_USER", ""),
            password: env_or("SOFTPHONE_SIP_PASSWORD", ""),
            domain: env_or("SOFTPHONE_SIP_DOMAIN", ""),
            proxy: env_or("SOFTPHONE_SIP_PROXY", ""),
            transport: env_or("SOFTPHONE_SIP_TRANSPORT", "tcp").to_ascii_lowercase(),
            http_addr: env_or("SOFTPHONE_HTTP_ADDR", DEFAULT_HTTP_ADDR),
            data_dir: env_or("SOFTPHONE_DATA_DIR", "recordings").into(),
            xai_key: env_or("SOFTPHONE_XAI_API_KEY", &env_or("XAI_API_KEY", "")),
            xai_model: env_or("SOFTPHONE_XAI_MODEL", "grok-voice-think-fast-2.0"),
            xai_voice: env_or("SOFTPHONE_XAI_VOICE", "ara"),
            auto_answer: if auto == "manual" { None } else { Some(Mode::parse(&auto).map_err(|e| anyhow::anyhow!("SOFTPHONE_AUTO_ANSWER: {e}"))?) },
            auto_answer_after: env_duration("SOFTPHONE_AUTO_ANSWER_AFTER", Duration::from_secs(2)),
            ring_timeout: env_duration("SOFTPHONE_RING_TIMEOUT", Duration::from_secs(60)),
        };
        let mut cfg = cfg;
        if cfg.proxy.is_empty() {
            cfg.proxy = cfg.domain.clone();
        }
        if cfg.user.is_empty() || cfg.password.is_empty() || cfg.domain.is_empty() {
            anyhow::bail!("SOFTPHONE_SIP_USER, SOFTPHONE_SIP_PASSWORD and SOFTPHONE_SIP_DOMAIN are required");
        }
        if cfg.transport != "tcp" && cfg.transport != "udp" {
            anyhow::bail!("SOFTPHONE_SIP_TRANSPORT must be tcp or udp");
        }
        Ok(cfg)
    }
}

pub fn env_or(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => fallback.to_string(),
    }
}

fn env_duration(key: &str, fallback: Duration) -> Duration {
    parse_duration(&env_or(key, "")).unwrap_or(fallback)
}

/// Go-style durations (`2s`, `500ms`, `1m`) or whole seconds.
pub fn parse_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (n, unit) = v.split_at(split);
    let n: f64 = n.parse().ok()?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
}

/// Loads `.env.local`, then `.env`, from the working directory.
pub fn load_env_files() {
    load_dot_env(".env.local");
    load_dot_env(".env");
}

/// Sets KEY=VALUE lines that are not already in the environment.
pub fn load_dot_env(path: &str) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if std::env::var_os(key).is_none() {
            // SAFETY: called from main before any other thread starts.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("2s"), Some(Duration::from_secs(2)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("1m"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("soon"), None);
    }
}
