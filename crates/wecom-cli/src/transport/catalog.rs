//! 自建内置 endpoint 目录（协议差异由产品层定义）。

use wecom::{EndpointCatalog, EndpointKey, PayloadStringReq};
use wecom_transport::HttpEndpoint;

use super::capability::AuthRequirement;
use super::envelope::NestedRes;

/// 自建内置 endpoint 配置目录：在 wecom crate 内建默认之上，为全部内置
/// endpoint 挂上网关扁平协议响应信封（[`NestedRes`]），并为媒体上传 /
/// schema 方法挂上 [`AuthRequirement`] 能力。
///
/// 协议差异由产品层定义：网关扁平协议默认实现位于本 crate（wecom-cli）。
pub fn endpoint_catalog() -> EndpointCatalog {
    EndpointCatalog::default().map_all(|key, ep| {
        let ep = ep.map::<HttpEndpoint>(|h| {
            h.with_req_envelope(PayloadStringReq)
                .with_res_envelope(NestedRes)
        });
        ep.with(AuthRequirement::new(key != EndpointKey::ServiceDiscovery))
    })
}
