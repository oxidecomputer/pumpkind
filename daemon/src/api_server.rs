// This Source Code list is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::convert::Into;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;

use dropshot::endpoint;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::Path;
use dropshot::RequestContext;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use slog::info;
use slog::o;

use crate::beacon;
use crate::Global;
use crate::SwitchId;

/// Return the direct contact data for all of the sleds this daemon is aware of
#[endpoint {
    method = GET,
    path = "/contacts",
}]
async fn get_sleds(
    rqctx: RequestContext<Arc<Global>>,
) -> Result<HttpResponseOk<Vec<beacon::SledReport>>, HttpError> {
    let g: &Global = rqctx.context();
    Ok(HttpResponseOk(
        g.sleds
            .lock()
            .unwrap()
            .values()
            .map(beacon::SledReport::from)
            .collect(),
    ))
}

/// When running on a sled, this returns the contact data for the two switches
/// it should be connected to.
#[endpoint {
    method = GET,
    path = "/switches",
}]
async fn get_switches(
    rqctx: RequestContext<Arc<Global>>,
) -> Result<HttpResponseOk<Vec<beacon::ContactReport>>, HttpError> {
    let g: &Global = rqctx.context();

    if g.switch_id.is_some() {
        return Err(HttpError::for_client_error_with_status(
            Some("switches have no connected switches".to_string()),
            dropshot::ClientErrorStatusCode::NOT_FOUND,
        ));
    }

    Ok(HttpResponseOk(
        g.switches
            .lock()
            .unwrap()
            .iter()
            .map(beacon::ContactReport::from)
            .collect(),
    ))
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct SledName {
    /// Name of the sled whose contact data is being requested
    sled_name: String,
}

/// Return the contact data reported by a single sled
#[endpoint {
    method = GET,
    path = "/sled/{sled_name}",
}]
async fn reports_get(
    rqctx: RequestContext<Arc<Global>>,
    path: Path<SledName>,
) -> Result<HttpResponseOk<Vec<beacon::SledReport>>, HttpError> {
    let g: &Global = rqctx.context();
    let sled_name = path.into_inner().sled_name;
    let data: Vec<beacon::SledReport> = g
        .matrix
        .lock()
        .unwrap()
        .iter()
        .filter(|((sled_a, _sled_b), _data)| sled_a == &sled_name)
        .map(|((_, _), data)| data.clone())
        .collect();

    if data.is_empty() {
        Err(HttpError::for_client_error_with_status(
            Some("switches have no connected switches".to_string()),
            dropshot::ClientErrorStatusCode::NOT_FOUND,
        ))
    } else {
        Ok(HttpResponseOk(data))
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
/// Represents a connection to a sled over a specific switch
struct SledSwitch {
    /// Name of the sled on the T6 side of the link
    sled_name: String,
    /// ID of the switch on the sidecar side of the link
    switch: SwitchId,
}

fn toggle_pumpkin_mode(
    g: &Global,
    sled: &str,
    switch: SwitchId,
    val: bool,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sled = sled.to_string();
    let mut pumpkins = g.pumpkins.lock().unwrap();
    if val {
        pumpkins.insert((sled, switch));
        Ok(HttpResponseUpdatedNoContent())
    } else if pumpkins.remove(&(sled.clone(), switch)) {
        Ok(HttpResponseUpdatedNoContent())
    } else {
        Err(HttpError::for_client_error_with_status(
            Some(format!("{sled}/{switch:?} not in pumpkin mode")),
            dropshot::ClientErrorStatusCode::NOT_FOUND,
        ))
    }
}

/// Stop sending beacons to a specific sled on a specific switch
#[endpoint {
    method = POST,
    path = "/pumpkin/{sled_name}/{switch}/on",
}]
async fn pumpkin_on(
    rqctx: RequestContext<Arc<Global>>,
    path: Path<SledSwitch>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let g: &Global = rqctx.context();
    let path = path.into_inner();
    toggle_pumpkin_mode(g, &path.sled_name, path.switch, true)
}

/// Restart sending beacons to a specific sled on a specific switch
#[endpoint {
    method = POST,
    path = "/pumpkin/{sled_name}/{switch}/off",
}]
async fn pumpkin_off(
    rqctx: RequestContext<Arc<Global>>,
    path: Path<SledSwitch>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let g: &Global = rqctx.context();
    let path = path.into_inner();
    toggle_pumpkin_mode(g, &path.sled_name, path.switch, false)
}

/// Get a list of all the (sled, switch) pairs to which we are currently
/// behaving like a pumpkin.
#[endpoint {
    method = GET,
    path = "/pumpkin",
}]
async fn pumpkin_list(
    rqctx: RequestContext<Arc<Global>>,
) -> Result<HttpResponseOk<Vec<SledSwitch>>, HttpError> {
    let g: &Global = rqctx.context();
    Ok(HttpResponseOk(
        g.pumpkins
            .lock()
            .unwrap()
            .iter()
            .map(|(sled, switch)| SledSwitch {
                sled_name: sled.to_string(),
                switch: *switch,
            })
            .collect(),
    ))
}

/// Trigger a diagnostic dump in the log
#[endpoint {
    method = POST,
    path = "/diags",
}]
async fn trigger_diagnostics(
    rqctx: RequestContext<Arc<Global>>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let g: &Global = rqctx.context();

    if g.switch_id.is_none() {
        Err(HttpError::for_client_error_with_status(
            Some("diagnostics are only available in a switch zone".to_string()),
            dropshot::ClientErrorStatusCode::NOT_FOUND,
        ))
    } else {
        crate::switch::record_diags(&g.log).await;
        Ok(HttpResponseUpdatedNoContent())
    }
}

/// Detailed build information about `pumpkind`.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct BuildInfo {
    pub version: String,
    pub git_sha: String,
    pub git_commit_timestamp: String,
    pub git_branch: String,
    pub rustc_semver: String,
    pub rustc_channel: String,
    pub rustc_host_triple: String,
    pub rustc_commit_sha: String,
    pub cargo_triple: String,
    pub debug: bool,
    pub opt_level: u8,
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            git_sha: env!("VERGEN_GIT_SHA").to_string(),
            git_commit_timestamp: env!("VERGEN_GIT_COMMIT_TIMESTAMP")
                .to_string(),
            git_branch: env!("VERGEN_GIT_BRANCH").to_string(),
            rustc_semver: env!("VERGEN_RUSTC_SEMVER").to_string(),
            rustc_channel: env!("VERGEN_RUSTC_CHANNEL").to_string(),
            rustc_host_triple: env!("VERGEN_RUSTC_HOST_TRIPLE").to_string(),
            rustc_commit_sha: env!("VERGEN_RUSTC_COMMIT_HASH").to_string(),
            cargo_triple: env!("VERGEN_CARGO_TARGET_TRIPLE").to_string(),
            debug: env!("VERGEN_CARGO_DEBUG").parse().unwrap(),
            opt_level: env!("VERGEN_CARGO_OPT_LEVEL").parse().unwrap(),
        }
    }
}

/// Return detailed build information about the `dpd` server itself.
#[endpoint {
    method = GET,
    path = "/build-info",
}]
async fn build_info(
    _rqctx: RequestContext<Arc<Global>>,
) -> Result<HttpResponseOk<BuildInfo>, HttpError> {
    Ok(HttpResponseOk(BuildInfo::default()))
}

pub async fn api_server(
    global: Arc<Global>,
    api_port: u16,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) -> anyhow::Result<()> {
    let log = global.log.new(o!("unit" => "api-server"));
    let bind_address =
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), api_port);

    slog::info!(log, "starting api server on {bind_address}");
    let config_dropshot = dropshot::ConfigDropshot {
        bind_address,
        default_request_body_max_bytes: 10240,
        default_handler_task_mode: dropshot::HandlerTaskMode::Detached,
        log_headers: vec![],
    };

    let server = dropshot::HttpServerStarter::new(
        &config_dropshot,
        http_api(),
        global.clone(),
        &log,
    )
    .map_err(|e| anyhow::anyhow!("failed to launch API server: {e:?}"))
    .map(|s| s.start())?;

    let _ = shutdown_rx.changed().await;
    info!(log, "closing api server");
    server
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("error closing api server: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::BuildInfo;
    use std::process::Command;

    #[test]
    fn test_build_info() {
        let info = BuildInfo::default();
        println!("{info:#?}");
        let out = Command::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .unwrap();
        assert!(out.status.success());
        let ours = std::str::from_utf8(&out.stdout).unwrap().trim();
        assert_eq!(info.git_sha, ours);
    }
}

pub fn http_api() -> dropshot::ApiDescription<Arc<Global>> {
    let mut api = dropshot::ApiDescription::new();

    api.register(build_info).unwrap();
    api.register(get_sleds).unwrap();
    api.register(get_switches).unwrap();
    api.register(reports_get).unwrap();
    api.register(pumpkin_on).unwrap();
    api.register(pumpkin_off).unwrap();
    api.register(pumpkin_list).unwrap();
    api.register(trigger_diagnostics).unwrap();

    api
}
