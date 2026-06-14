//! Central configuration for Signal + WhatsApp Hub.
//!
//! All the CLI paths, the phone number, and the Java home needed by signal-cli
//! live here. Everything is driven by environment variables so the app is
//! portable across machines — nothing is hardcoded to a particular user.
//!
//! Set these before launching (e.g. in a `.env`, your shell profile, or the
//! systemd unit):
//!
//! ```text
//!   HUB_WA_CLI         path to the whatsmeow `whatsapp-cli` binary
//!   HUB_WA_AUTH        directory holding the WhatsApp session (whatsapp.db)
//!   HUB_WA_ACCOUNT     your WhatsApp number in +E.164 (e.g. +15551234567)
//!   HUB_SIGNAL_CLI     path to the `signal-cli` launcher script
//!   HUB_SIGNAL_CONFIG  signal-cli --config dir   (default: ~/.local/share/signal-cli)
//!   HUB_SIGNAL_ACCOUNT your Signal number in +E.164
//!   HUB_JAVA_HOME      JAVA_HOME for signal-cli  (signal-cli 0.14.x needs Java 25)
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn env_or(key: &str, default: PathBuf) -> PathBuf {
    std::env::var(key).map(PathBuf::from).unwrap_or(default)
}

fn env_or_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Best-effort default JAVA_HOME. Prefer an explicit `HUB_JAVA_HOME`; this only
/// guesses common Linux OpenJDK install locations so the app can run unconfigured.
fn default_java_home() -> PathBuf {
    for candidate in [
        "/usr/lib/jvm/java-25-openjdk",
        "/usr/lib/jvm/java-25",
        "/usr/lib/jvm/default-java",
        "/opt/java",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_dir() {
            return p;
        }
    }
    // Fall back to whatever JAVA_HOME is already in the environment, else empty
    // (the app degrades gracefully and just reports Signal as not reachable).
    std::env::var("JAVA_HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the whatsmeow-based whatsapp-cli Go binary.
    pub whatsapp_cli: PathBuf,
    /// Directory containing the WhatsApp session store (whatsapp.db).
    pub whatsapp_auth: PathBuf,
    /// The WhatsApp number, +E.164.
    pub whatsapp_account: String,

    /// Path to the signal-cli launcher script.
    pub signal_cli: PathBuf,
    /// --config dir for signal-cli (account data store lives under here).
    pub signal_config_dir: PathBuf,
    /// The Signal account, +E.164.
    pub signal_account: String,
    /// JAVA_HOME for signal-cli (0.14.x needs Java 25+).
    pub java_home: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Config {
            // No portable default exists for these binaries — point the env vars
            // at wherever you installed whatsapp-cli / signal-cli.
            whatsapp_cli: env_or("HUB_WA_CLI", PathBuf::from("whatsapp-cli")),
            whatsapp_auth: env_or(
                "HUB_WA_AUTH",
                home.join(".local/share/whatsapp-hub/auth"),
            ),
            whatsapp_account: env_or_str("HUB_WA_ACCOUNT", ""),

            signal_cli: env_or("HUB_SIGNAL_CLI", PathBuf::from("signal-cli")),
            signal_config_dir: env_or(
                "HUB_SIGNAL_CONFIG",
                home.join(".local/share/signal-cli"),
            ),
            signal_account: env_or_str("HUB_SIGNAL_ACCOUNT", ""),
            java_home: env_or("HUB_JAVA_HOME", default_java_home()),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        Config::default()
    }
}
