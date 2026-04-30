//! REAPI `Capabilities` service. Returns a static, Phase-1-appropriate set.

use brokkr_proto::reapi_v2::{self as rapi, capabilities_server::Capabilities as CapSvc};
use tonic::{Request, Response, Status};

/// REAPI `Capabilities` service. Returns a static, Phase-1-appropriate set.
#[derive(Default)]
pub struct CapabilitiesService;

#[tonic::async_trait]
impl CapSvc for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _request: Request<rapi::GetCapabilitiesRequest>,
    ) -> Result<Response<rapi::ServerCapabilities>, Status> {
        let span = tracing::info_span!("capabilities::get_capabilities");
        let _enter = span.enter();
        let caps = rapi::ServerCapabilities {
            cache_capabilities: Some(rapi::CacheCapabilities {
                digest_functions: vec![rapi::digest_function::Value::Sha256 as i32],
                action_cache_update_capabilities: Some(rapi::ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                max_batch_total_size_bytes: 4 * 1024 * 1024,
                symlink_absolute_path_strategy:
                    rapi::symlink_absolute_path_strategy::Value::Disallowed as i32,
                ..Default::default()
            }),
            execution_capabilities: Some(rapi::ExecutionCapabilities {
                digest_function: rapi::digest_function::Value::Sha256 as i32,
                exec_enabled: true,
                digest_functions: vec![rapi::digest_function::Value::Sha256 as i32],
                ..Default::default()
            }),
            low_api_version: Some(brokkr_proto::semver::SemVer {
                major: 2,
                ..Default::default()
            }),
            high_api_version: Some(brokkr_proto::semver::SemVer {
                major: 2,
                minor: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(Response::new(caps))
    }
}
