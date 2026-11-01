//! Integrates local application settings.
use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::Value;

use crate::{Error, Result};

const NO_PROXY_KEY: &str = "http.noProxy";
const KEYS: [&str; 4] = [
    "http.proxy",
    "http.proxySupport",
    "cursor.general.disableHttp2",
    "http.experimental.systemCertificatesV2",
];
/// Older builds wrote the local proxy URL into the Kerberos SPN setting. It
/// must not survive, but a real user-configured SPN should be preserved.
const LEGACY_KERBEROS_KEY: &str = "http.proxyKerberosServicePrincipal";

fn path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => Ok(home.join("Library/Application Support/Cursor/User/settings.json")),
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/settings.json")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/settings.json")),
        platform => Err(Error::Config(format!(
            "Cursor settings are unsupported on {platform}"
        ))),
    }
}

fn read() -> Result<BTreeMap<String, Value>> {
    let path = path()?;
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if data.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    json5::from_str(&data)
        .map_err(|error| Error::Config(format!("parse Cursor settings JSONC: {error}")))
}

fn write(settings: &BTreeMap<String, Value>) -> Result<()> {
    let path = path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, [data.as_slice(), b"\n"].concat())?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn proxy_url() -> Result<Option<String>> {
    Ok(read()?
        .get(KEYS[0])
        .and_then(Value::as_str)
        .map(str::to_owned))
}

pub fn write_proxy_settings(proxy_url: &str) -> Result<()> {
    let mut settings = read()?;
    settings.remove(NO_PROXY_KEY);
    remove_legacy_kerberos_setting(&mut settings);
    settings.insert(KEYS[0].into(), Value::String(proxy_url.into()));
    settings.insert(KEYS[1].into(), Value::String("on".into()));
    settings.insert(KEYS[2].into(), Value::Bool(true));
    settings.insert(KEYS[3].into(), Value::Bool(true));
    write(&settings)
}

pub fn clear_proxy_settings() -> Result<()> {
    let mut settings = read()?;
    let before = settings.len();
    for key in KEYS {
        settings.remove(key);
    }
    remove_legacy_kerberos_setting(&mut settings);
    if settings.len() != before {
        write(&settings)?;
    }
    Ok(())
}

pub fn settings_match(proxy_url: &str) -> Result<bool> {
    let settings = read()?;
    Ok(
        settings.get(KEYS[0]) == Some(&Value::String(proxy_url.into()))
            && settings.get(KEYS[1]) == Some(&Value::String("on".into()))
            && settings.get(KEYS[2]) == Some(&Value::Bool(true))
            && settings.get(KEYS[3]) == Some(&Value::Bool(true)),
    )
}

fn remove_legacy_kerberos_setting(settings: &mut BTreeMap<String, Value>) {
    let managed = settings
        .get(LEGACY_KERBEROS_KEY)
        .and_then(Value::as_str)
        .and_then(|value| reqwest::Url::parse(value).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
    if managed {
        settings.remove(LEGACY_KERBEROS_KEY);
    }
}

pub fn clear_stale_managed_settings() -> Result<()> {
    let mut settings = read()?;
    let before = settings.len();
    let managed_signature = settings.get(KEYS[1]) == Some(&Value::String("on".into()))
        && settings.get(KEYS[2]) == Some(&Value::Bool(true))
        && settings.get(KEYS[3]) == Some(&Value::Bool(true));
    let loopback = settings
        .get(KEYS[0])
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<reqwest::Url>().ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
    if managed_signature && loopback {
        for key in KEYS {
            settings.remove(key);
        }
    }
    remove_legacy_kerberos_setting(&mut settings);
    if settings.len() != before {
        write(&settings)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{remove_legacy_kerberos_setting, LEGACY_KERBEROS_KEY};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    #[test]
    fn removes_only_managed_loopback_kerberos_values() {
        let mut settings = BTreeMap::from([(
            LEGACY_KERBEROS_KEY.to_owned(),
            json!("http://127.0.0.1:58085"),
        )]);
        remove_legacy_kerberos_setting(&mut settings);
        assert!(settings.is_empty());

        let mut settings = BTreeMap::from([(
            LEGACY_KERBEROS_KEY.to_owned(),
            Value::String("HTTP/kerberos.example.com".into()),
        )]);
        remove_legacy_kerberos_setting(&mut settings);
        assert!(settings.contains_key(LEGACY_KERBEROS_KEY));
    }
}
