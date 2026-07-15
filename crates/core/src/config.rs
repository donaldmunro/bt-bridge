//! Persistent configuration shared by the CLI and the GUI.
//!
//! Lives at `$XDG_CONFIG_HOME/bt-bridge/config.toml` (default
//! `~/.config/bt-bridge/config.toml`):
//!
//! ```toml
//! reconnect = true
//! ```
//!
//! Unknown keys (e.g. the removed `prefer` routing preferences from merged mode) are
//! ignored, so old config files keep loading.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config
{
   /// Actively reconnect bridged devices that drop their BLE link (see `--reconnect`).
   #[serde(default)]
   pub reconnect: bool,
}

/// `$XDG_CONFIG_HOME/bt-bridge/config.toml`, falling back to `~/.config`.
pub fn config_path() -> Result<PathBuf>
{
   let base = std::env::var_os("XDG_CONFIG_HOME")
                 .map(PathBuf::from)
                 .filter(|p| p.is_absolute())
                 .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                 .context("neither XDG_CONFIG_HOME nor HOME is set")?;
   Ok(base.join("bt-bridge").join("config.toml"))
}

/// Load the configuration; a missing file yields the default (empty) configuration,
/// a malformed one is an error (the user edited it - do not silently drop their intent).
pub fn load() -> Result<Config> { load_from(&config_path()?) }

pub fn load_from(path: &Path) -> Result<Config>
{
   match std::fs::read_to_string(path)
   {
      | Ok(text) => toml::from_str(&text).with_context(|| format!("malformed config file {}", path.display())),
      | Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
      | Err(e) => Err(e).with_context(|| format!("cannot read config file {}", path.display())),
   }
}

pub fn save(config: &Config) -> Result<()> { save_to(&config_path()?, config) }

pub fn save_to(path: &Path, config: &Config) -> Result<()>
{
   if let Some(parent) = path.parent()
   {
      std::fs::create_dir_all(parent)
         .with_context(|| format!("cannot create config directory {}", parent.display()))?;
   }
   let text = toml::to_string_pretty(config).context("cannot serialize config")?;
   std::fs::write(path, text).with_context(|| format!("cannot write config file {}", path.display()))
}

#[cfg(test)]
mod tests
{
   use super::*;

   fn temp_config_path(tag: &str) -> PathBuf
   {
      std::env::temp_dir().join(format!("bt-bridge-test-{tag}-{}", std::process::id()))
                          .join("config.toml")
   }

   #[test]
   fn round_trip_and_missing_file()
   {
      let path = temp_config_path("roundtrip");
      // Missing file → default.
      assert_eq!(load_from(&path).unwrap(), Config::default());

      let config = Config { reconnect: true };
      save_to(&path, &config).unwrap();
      assert_eq!(load_from(&path).unwrap(), config);

      std::fs::remove_dir_all(path.parent().unwrap()).ok();
   }

   #[test]
   fn old_config_with_prefer_entries_still_loads()
   {
      // Merged mode persisted `prefer = [...]`; the field is gone but old files must
      // keep loading (unknown keys ignored).
      let path = temp_config_path("legacy-prefer");
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "prefer = [\"AA:BB:CC:DD:EE:FF:0x1818\"]\nreconnect = true\n").unwrap();
      assert_eq!(load_from(&path).unwrap(), Config { reconnect: true });
      std::fs::remove_dir_all(path.parent().unwrap()).ok();
   }

   #[test]
   fn malformed_file_is_an_error()
   {
      let path = temp_config_path("malformed");
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "reconnect = \"not-a-bool\"").unwrap();
      assert!(load_from(&path).is_err());
      std::fs::remove_dir_all(path.parent().unwrap()).ok();
   }
}
