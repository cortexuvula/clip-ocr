use crate::core::EngineId;
use serde::{Deserialize, Serialize};
use std::path::Path;
pub const DEFAULT_HOTKEY: &str = "CmdOrCtrl+Alt+O";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub hotkey: String,
    pub engine: EngineId,
    pub language: String,
    pub collapse_line_breaks: bool,
    pub launch_at_login: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.into(),
            engine: EngineId::MacVision,
            language: "en-US".into(),
            collapse_line_breaks: false,
            launch_at_login: false,
        }
    }
}
pub fn resolve_hotkey(s: &str) -> &str {
    if s.trim().is_empty() {
        DEFAULT_HOTKEY
    } else {
        s.trim()
    }
}
pub fn validate_hotkey(s: &str) -> Result<(), String> {
    use std::str::FromStr;
    tauri_plugin_global_shortcut::Shortcut::from_str(resolve_hotkey(s))
        .map(|_| ())
        .map_err(|_| "Invalid hotkey; use e.g. CmdOrCtrl+Alt+O".into())
}
impl Config {
    pub fn validate(&self, languages: &[String]) -> Result<(), String> {
        validate_hotkey(&self.hotkey)?;
        if self.engine != EngineId::MacVision || !cfg!(target_os = "macos") {
            return Err("Engine is not shipped on this platform".into());
        }
        if !languages.contains(&self.language) {
            return Err("Language is not supported by this OS's Vision engine".into());
        }
        Ok(())
    }
    pub fn load(root: &Path) -> Result<Self, String> {
        use std::io::Read;
        let path = root.join("config.toml");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(file) => {
                let mut text = String::new();
                file.take(16_385)
                    .read_to_string(&mut text)
                    .map_err(|_| "Cannot read config.toml")?;
                if text.len() > 16_384 {
                    return Err("Config is too large".into());
                }
                toml::from_str(&text)
                    .map_err(|_| "Invalid config.toml (offline engine allowlist enforced)".into())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Cannot read config.toml".into()),
        }
    }
    pub fn save(&self, root: &Path) -> Result<(), String> {
        use std::io::Write;
        let text = toml::to_string_pretty(self).map_err(|_| "Cannot serialize settings")?;
        let path = root.join(format!("config-{}.tmp", uuid::Uuid::new_v4()));
        let _cleanup = crate::storage::RemoveOnDrop(path.clone());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).map_err(|_| "Cannot create settings")?;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot write settings")?;
        std::fs::rename(&path, root.join("config.toml")).map_err(|_| "Cannot replace settings")?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hotkey_resolves_and_validates() {
        for s in ["", " ", DEFAULT_HOTKEY, "Ctrl+Shift+O"] {
            assert!(validate_hotkey(s).is_ok());
        }
        assert_eq!(resolve_hotkey(" "), DEFAULT_HOTKEY);
        assert_eq!(resolve_hotkey(" Ctrl+Shift+O "), "Ctrl+Shift+O");
        assert!(validate_hotkey("not a shortcut").is_err());
    }
    #[test]
    fn config_roundtrip_and_remote_override_rejection() {
        let t = tempfile::tempdir().unwrap();
        let c = Config::default();
        c.save(t.path()).unwrap();
        assert_eq!(Config::load(t.path()).unwrap(), c);
        assert!(toml::from_str::<Config>("engine = 'cloud'").is_err());
        assert!(toml::from_str::<Config>("provider = 'remote'").is_err());
        assert!(c.validate(&[]).is_err());
    }
}
