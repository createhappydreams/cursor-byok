//! Integrates local application settings.
use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::Value;

use crate::{cursor::services::server_config, Error, Result};

const NO_PROXY_KEY: &str = "http.noProxy";
const PROXY_KEY: &str = "http.proxy";
const PROXY_SUPPORT_KEY: &str = "http.proxySupport";
const DISABLE_HTTP2_KEY: &str = "cursor.general.disableHttp2";
const SYSTEM_CERTS_KEY: &str = "http.experimental.systemCertificatesV2";
/// Keys this application manages and rewrites on every start.
const KEYS: [&str; 4] = [PROXY_KEY, PROXY_SUPPORT_KEY, DISABLE_HTTP2_KEY, SYSTEM_CERTS_KEY];
/// `http.proxyKerberosServicePrincipal` holds an SPN (`HTTP/proxy`), not a proxy URL.
/// Earlier builds wrote the URL here, which is not a valid value for the key, so it
/// is dropped whenever the managed block is written or cleared.
const STALE_KEYS: [&str; 1] = ["http.proxyKerberosServicePrincipal"];

/// The workspace applies this setting locally when the proxy is enabled, and a local
/// `true` overrides whatever `GetServerConfigResponse` asks for. Track the same
/// policy the server serves, so `CURSOR_BYOK_HTTP2` switches both sides at once and
/// a value left over from an older version is rewritten on the next start.
fn managed_disable_http2() -> bool {
    !server_config::http2_allowed()
}

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

pub fn write_proxy_settings(proxy_url: &str) -> Result<()> {
    let mut settings = read()?;
    settings.remove(NO_PROXY_KEY);
    for key in STALE_KEYS {
        settings.remove(key);
    }
    settings.insert(PROXY_KEY.into(), Value::String(proxy_url.into()));
    settings.insert(PROXY_SUPPORT_KEY.into(), Value::String("on".into()));
    settings.insert(
        DISABLE_HTTP2_KEY.into(),
        Value::Bool(managed_disable_http2()),
    );
    settings.insert(SYSTEM_CERTS_KEY.into(), Value::Bool(true));
    write(&settings)
}

pub fn clear_proxy_settings() -> Result<()> {
    let mut settings = read()?;
    let before = settings.len();
    for key in KEYS.into_iter().chain(STALE_KEYS) {
        settings.remove(key);
    }
    if settings.len() != before {
        write(&settings)?;
    }
    Ok(())
}

pub fn settings_match(proxy_url: &str) -> Result<bool> {
    let settings = read()?;
    Ok(
        settings.get(PROXY_KEY) == Some(&Value::String(proxy_url.into()))
            && settings.get(PROXY_SUPPORT_KEY) == Some(&Value::String("on".into()))
            && settings.get(DISABLE_HTTP2_KEY) == Some(&Value::Bool(managed_disable_http2()))
            && settings.get(SYSTEM_CERTS_KEY) == Some(&Value::Bool(true)),
    )
}

pub fn clear_stale_managed_settings() -> Result<()> {
    let settings = read()?;
    let managed_signature = settings.get(PROXY_SUPPORT_KEY) == Some(&Value::String("on".into()))
        && settings.get(DISABLE_HTTP2_KEY) == Some(&Value::Bool(managed_disable_http2()))
        && settings.get(SYSTEM_CERTS_KEY) == Some(&Value::Bool(true));
    let loopback = settings
        .get(PROXY_KEY)
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<reqwest::Url>().ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
    if managed_signature && loopback {
        clear_proxy_settings()?;
    }
    Ok(())
}
