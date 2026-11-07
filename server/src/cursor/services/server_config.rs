//! Keeps Cursor Agent traffic on the endpoint selected with `agent -e`.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use prost::Message;

use crate::Result;

const HTTP2_CONFIG_FORCE_ALL_DISABLED: i32 = 1;

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
        http2_config: HTTP2_CONFIG_FORCE_ALL_DISABLED,
        cli_sandbox_default_enabled: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forces_agent_cli_to_use_the_selected_legacy_endpoint() {
        let config = server_config();
        assert_eq!(config.http2_config, HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(config.cli_sandbox_default_enabled, Some(true));
    }

    #[test]
    fn allows_local_experiment_overrides() {
        assert!(
            server_config().is_dev_do_not_use_for_secret_things_because_can_be_spoofed_by_users
        );
    }
}
