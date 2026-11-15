//! Keeps Cursor Agent traffic on the endpoint selected with `agent -e`.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use prost::Message;

use crate::Result;

// aiserver.v1.Http2Config. The workbench reads this back from
// GetServerConfigResponse and rebuilds every agent transport from it — its own log
// says `transports rebuilt (init): none -> http1_sse, http1, reason
// server_force_all`. Forcing ALL_DISABLED multiplexes the bidi stream over SSE and
// drops the RemoteAgentHost tunnel (`tunnel ended with code 4`) every 20-30s; a
// dropped tunnel cancels every in-flight KV SET, which is how a single agent turn
// could stall for minutes before failing with `KV SET <id>: Canceled`.
//
// Leaving it UNSPECIFIED hands the decision back to the client's own default
// policy. The FORCE_ALL_DISABLED pin existed to keep agent traffic on the endpoint
// selected by `agent -e`; after changing it, confirm from the workbench log what
// `transports rebuilt ... reason ...` reports instead of assuming.
const HTTP2_CONFIG_UNSPECIFIED: i32 = 0;
const HTTP2_CONFIG_FORCE_ALL_DISABLED: i32 = 1;
const HTTP2_CONFIG_FORCE_ALL_ENABLED: i32 = 2;
const HTTP2_CONFIG_FORCE_BIDI_DISABLED: i32 = 3;
const HTTP2_CONFIG_FORCE_BIDI_ENABLED: i32 = 4;

/// Runtime override so this can be reverted without rebuilding the binary:
/// `CURSOR_BYOK_HTTP2=default|off|on|bidi_off|bidi_on`, or `0`..=`4`.
/// The workbench caches the response, so Cursor still has to be restarted for a
/// changed value to take effect.
const HTTP2_CONFIG_ENV: &str = "CURSOR_BYOK_HTTP2";

// field 2 of aiserver.v1.GetServerConfigResponse. The workbench caches the whole
// response as `cursorai/serverConfig` and reads this flag back when deciding
// whether local experiment overrides are allowed; the field name is Cursor's own
// wording ("do not use for secret things, users can spoof it").
const DEV_USER_FLAG_SPOOFABLE_BY_DESIGN: bool = true;

#[derive(Clone, PartialEq, Message)]
struct ServerConfigResponse {
    // Marks the profile as a Cursor developer build so `ExperimentService`
    // honours `workbench.experiments.featureFlagOverrides`. Without it the
    // renderer ignores every local gate override, and Cursor keeps taking the
    // server-driven `agent_host_local_loop` path, which speaks
    // aiserver.v1.RunInferenceRunRequest over BidiAppend — a protocol this
    // server does not implement.
    #[prost(bool, tag = "2")]
    is_dev_do_not_use_for_secret_things_because_can_be_spoofed_by_users: bool,
    #[prost(string, tag = "6")]
    config_version: String,
    #[prost(int32, tag = "7")]
    http2_config: i32,
    #[prost(bool, optional, tag = "28")]
    cli_sandbox_default_enabled: Option<bool>,
}

pub async fn get() -> Result<Response<Body>> {
    let payload = server_config().encode_to_vec();
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

fn server_config() -> ServerConfigResponse {
    ServerConfigResponse {
        is_dev_do_not_use_for_secret_things_because_can_be_spoofed_by_users:
            DEV_USER_FLAG_SPOOFABLE_BY_DESIGN,
        config_version: "cursor_byok_local_agent_v1".into(),
        http2_config: http2_config(),
        cli_sandbox_default_enabled: Some(true),
    }
}

pub(crate) fn http2_config() -> i32 {
    parse_http2_config(std::env::var(HTTP2_CONFIG_ENV).ok().as_deref())
}

/// Whether the agent transports may negotiate HTTP/2.
///
/// Cursor's own `cursor.general.disableHttp2` setting has to agree with this: the
/// workspace applies it locally when the proxy is enabled, and a stale `true` there
/// overrides whatever this response asks for. Keeping both sides on one switch is
/// what makes `CURSOR_BYOK_HTTP2=off` a complete revert.
pub(crate) fn http2_allowed() -> bool {
    matches!(
        http2_config(),
        HTTP2_CONFIG_UNSPECIFIED | HTTP2_CONFIG_FORCE_ALL_ENABLED | HTTP2_CONFIG_FORCE_BIDI_ENABLED
    )
}

/// Unknown values fall back to the default instead of failing the request: a typo in
/// the override must not be able to break the agent transport at startup.
///
/// The default is HTTP/1.1 on purpose. The local MITM proxy can only inspect
/// HTTP/1.1 requests to decide whether a path belongs to the BYOK backend or to the
/// Cursor upstream; an HTTP/2 connection is tunnelled opaquely, so agent runs would
/// go straight to Cursor, which then rejects the BYOK model hash with
/// `ERROR_BAD_MODEL_NAME`. Only opt back in explicitly with
/// `CURSOR_BYOK_HTTP2=default|on|bidi_on`.
fn parse_http2_config(override_value: Option<&str>) -> i32 {
    match override_value.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) => match value.as_str() {
            "1" | "off" | "disabled" => HTTP2_CONFIG_FORCE_ALL_DISABLED,
            "2" | "on" | "enabled" => HTTP2_CONFIG_FORCE_ALL_ENABLED,
            "3" | "bidi_off" | "bidi_disabled" => HTTP2_CONFIG_FORCE_BIDI_DISABLED,
            "4" | "bidi_on" | "bidi_enabled" => HTTP2_CONFIG_FORCE_BIDI_ENABLED,
            "" | "0" | "default" | "unspecified" => HTTP2_CONFIG_UNSPECIFIED,
            other => {
                tracing::warn!(
                    value = other,
                    env = HTTP2_CONFIG_ENV,
                    "unrecognised http2 override, forcing http1"
                );
                HTTP2_CONFIG_FORCE_ALL_DISABLED
            }
        },
        None => HTTP2_CONFIG_FORCE_ALL_DISABLED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forces_http1_by_default() {
        let config = server_config();
        assert_eq!(config.cli_sandbox_default_enabled, Some(true));
        assert_eq!(parse_http2_config(None), HTTP2_CONFIG_FORCE_ALL_DISABLED);
    }

    #[test]
    fn maps_every_documented_override() {
        assert_eq!(parse_http2_config(Some("default")), HTTP2_CONFIG_UNSPECIFIED);
        assert_eq!(parse_http2_config(Some("0")), HTTP2_CONFIG_UNSPECIFIED);
        assert_eq!(parse_http2_config(Some(" off ")), HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(parse_http2_config(Some("1")), HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(parse_http2_config(Some("ON")), HTTP2_CONFIG_FORCE_ALL_ENABLED);
        assert_eq!(parse_http2_config(Some("2")), HTTP2_CONFIG_FORCE_ALL_ENABLED);
        assert_eq!(parse_http2_config(Some("3")), HTTP2_CONFIG_FORCE_BIDI_DISABLED);
        assert_eq!(parse_http2_config(Some("bidi_on")), HTTP2_CONFIG_FORCE_BIDI_ENABLED);
        assert_eq!(parse_http2_config(Some("4")), HTTP2_CONFIG_FORCE_BIDI_ENABLED);
    }

    #[test]
    fn unknown_override_falls_back_to_http1() {
        assert_eq!(parse_http2_config(Some("http3")), HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(parse_http2_config(Some("")), HTTP2_CONFIG_UNSPECIFIED);
    }

    #[test]
    fn allows_local_experiment_overrides() {
        assert!(
            server_config().is_dev_do_not_use_for_secret_things_because_can_be_spoofed_by_users
        );
    }
}
