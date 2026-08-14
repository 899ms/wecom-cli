// Used by config/logging tests gated behind feature flags.
#![allow(dead_code)]

use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::json;

use super::mock_builders::*;

/// Setup standard discovery mocks: catalog + hr service detail.
///
/// `ServiceDiscovery` 端点经 wecom-cli 端点目录挂 `PayloadStringReq` 网关信封，
/// 请求体为 `{"payload": "<json 字符串>"}` 包裹形态，matcher 按包裹后的请求体精确分流。
pub async fn setup_discovery_mocks(server: &mut Server) -> (Mock, Mock) {
    let catalog = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(&json!({}))))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(catalog_body())
        .create_async()
        .await;

    let hr = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(&json!({"service": "hr"}))))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(hr_service_body(&server.url()))
        .create_async()
        .await;

    (catalog, hr)
}

/// Setup a method call mock that returns a JSON response.
pub async fn setup_method_mock(server: &mut Server, path: &str, response_body: &str) -> Mock {
    server
        .mock("POST", path)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(response_body)
        .create_async()
        .await
}

/// Options for customizing discovery mocks.
pub struct DiscoveryMockOptions<'a> {
    /// Header matchers: `(header_name, expected_value)`.
    pub header_matchers: Vec<(&'a str, &'a str)>,
    /// If set, assert each mock is called exactly this many times.
    pub expect_count: Option<usize>,
}

/// Setup standard discovery mocks with custom header matchers and expect counts.
pub async fn setup_discovery_mocks_with<'a>(
    server: &mut Server,
    opts: DiscoveryMockOptions<'a>,
) -> (Mock, Mock) {
    let mut catalog_builder = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(&json!({}))))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(catalog_body());
    for (k, v) in &opts.header_matchers {
        catalog_builder = catalog_builder.match_header(*k, *v);
    }
    if let Some(n) = opts.expect_count {
        catalog_builder = catalog_builder.expect(n);
    }
    let catalog = catalog_builder.create_async().await;

    let mut hr_builder = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(&json!({"service": "hr"}))))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(hr_service_body(&server.url()));
    for (k, v) in &opts.header_matchers {
        hr_builder = hr_builder.match_header(*k, *v);
    }
    if let Some(n) = opts.expect_count {
        hr_builder = hr_builder.expect(n);
    }
    let hr = hr_builder.create_async().await;

    (catalog, hr)
}

/// Setup custom service discovery mocks (catalog + service detail).
pub async fn setup_custom_discovery_mocks(
    server: &mut Server,
    service_name: &str,
    description: &str,
    service_body: &str,
) -> (Mock, Mock) {
    let catalog = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(&json!({}))))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(custom_catalog_body(service_name, description))
        .create_async()
        .await;

    let svc = server
        .mock("POST", "/service/discovery")
        .match_body(Matcher::Json(payload_wrap(
            &json!({"service": service_name}),
        )))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(service_body)
        .create_async()
        .await;

    (catalog, svc)
}

/// Setup a sync mock server with standard discovery mocks.
///
/// Returns `(server_url, server)`. The caller must keep `server` alive.
#[cfg(feature = "custom-endpoint")]
pub fn setup_sync_discovery_server() -> (String, ServerGuard) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut server = Server::new_async().await;
        let (_catalog, _hr) = setup_discovery_mocks(&mut server).await;
        let url = server.url();
        // Forget the mocks so they stay alive with the server.
        std::mem::forget(_catalog);
        std::mem::forget(_hr);
        (url, server)
    })
}
