//! Persistent settings, saved accounts, and app config paths.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedAccount {
    pub account_name: String,
    pub steamid: u64,
    pub refresh_token: String,
    #[serde(default)]
    pub guard_data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Default install directory for new downloads.
    pub install_dir: String,
    /// Parallel CDN connections per task.
    pub max_connections: usize,
    /// Depot language filter (empty = only language-neutral depots).
    pub language: String,
    /// Include all language depots.
    pub all_languages: bool,
    /// Preferred OS for auto depot selection.
    pub os: String,
    /// Preferred architecture ("64" / "32" / "").
    pub arch: String,
    /// Steam region cell id (0 = auto).
    pub cell_id: u32,
    /// Saved accounts (refresh tokens).
    pub accounts: Vec<SavedAccount>,
    /// Last used account name.
    pub last_account: Option<String>,
    /// Machine-id seed (random hex), persisted for stable machine identity.
    pub machine_id_seed: String,
    /// Cached CM websocket endpoints.
    pub cm_endpoints: Vec<String>,
    /// Remembered UI filters.
    pub file_filters: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            install_dir: String::new(),
            max_connections: 16,
            language: default_language(),
            all_languages: false,
            os: default_os(),
            arch: "64".into(),
            cell_id: 0,
            accounts: Vec::new(),
            last_account: None,
            machine_id_seed: String::new(),
            cm_endpoints: Vec::new(),
            file_filters: String::new(),
        }
    }
}

fn default_os() -> String {
    #[cfg(target_os = "windows")]
    return "windows".into();
    #[cfg(target_os = "macos")]
    return "macos".into();
    #[cfg(all(unix, not(target_os = "macos")))]
    return "linux".into();
}

fn default_language() -> String {
    "english".into()
}

/// Config directory: <exe dir>/config, falling back to system config dir.
pub fn config_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let portable = dir.join("config");
            if portable.exists() || is_writable_dir(dir) {
                let _ = std::fs::create_dir_all(&portable);
                return portable;
            }
        }
    }
    let dir = dirs_config().join("DepotManager");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn dirs_config() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".config"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

fn is_writable_dir(dir: &std::path::Path) -> bool {
    let probe = dir.join(".dm-write-probe");
    match std::fs::write(&probe, b"1") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl Settings {
    pub fn load() -> Settings {
        let path = config_dir().join("settings.json");
        let mut s: Settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if s.machine_id_seed.is_empty() {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            s.machine_id_seed = hex::encode(b);
        }
        s
    }

    pub fn save(&self) {
        let path = config_dir().join("settings.json");
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn machine_id(&self) -> Vec<u8> {
        let seed = hex::decode(&self.machine_id_seed).unwrap_or_else(|_| b"depotmanager".to_vec());
        crate::steamkit::machineid::build_machine_id(&seed)
    }

    pub fn find_account(&self, name: &str) -> Option<&SavedAccount> {
        self.accounts.iter().find(|a| a.account_name.eq_ignore_ascii_case(name))
    }

    pub fn upsert_account(&mut self, account: SavedAccount) {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|a| a.account_name.eq_ignore_ascii_case(&account.account_name))
        {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove_account(&mut self, name: &str) {
        self.accounts.retain(|a| !a.account_name.eq_ignore_ascii_case(name));
        if self.last_account.as_deref().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false) {
            self.last_account = None;
        }
    }
}
