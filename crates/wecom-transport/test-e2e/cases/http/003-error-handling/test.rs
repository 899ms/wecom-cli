use std::sync::{Arc, Mutex};

use tracing_subscriber::prelude::*;
use wecom_transport::telemetry::{CaptureScope, HttpRequestRecord};

/// Collect spans into a Vec through on_request.
fn collect_spans(scope: &CaptureScope) -> Arc<Mutex<Vec<HttpRequestRecord>>> {
    let collected: Arc<Mutex<Vec<HttpRequestRecord>>> = Default::default();
    let c = collected.clone();
    scope.on_request(move |s: HttpRequestRecord| {
        c.lock().unwrap().push(s);
    });
    collected
}

#[tokio::test]
async fn http_500() {
    let server = MockServer::start().await;
    let transport = Transport::from(HttpTransportBackend::default());

    Mock::given(method("POST"))
        .and(path("/cgi-bin/fail"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let payload = json!({});
    let err = transport
        .invoke(ep(&server.uri(), "/cgi-bin/fail"), payload)
        .await
        .unwrap_err();

    match &err {
        Error::Http { status, .. } => assert_eq!(*status, 500),
        other => panic!("Expected Error::Http, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_404() {
    let server = MockServer::start().await;
    let transport = Transport::from(HttpTransportBackend::default());

    Mock::given(method("POST"))
        .and(path("/cgi-bin/not-found"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let payload = json!({});
    let err = transport
        .invoke(ep(&server.uri(), "/cgi-bin/not-found"), payload)
        .await
        .unwrap_err();

    match &err {
        Error::Http { status, .. } => assert_eq!(*status, 404),
        other => panic!("Expected Error::Http, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_business_error() {
    let server = MockServer::start().await;
    let transport = Transport::from(HttpTransportBackend::default());

    Mock::given(method("POST"))
        .and(path("/cgi-bin/biz-err"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(http_api_error_response(40001, "invalid token")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let payload = json!({});
    let err = transport
        .invoke(ep(&server.uri(), "/cgi-bin/biz-err"), payload)
        .await
        .unwrap_err();

    match &err {
        Error::Api { code, message, .. } => {
            assert_eq!(*code, Some(40001));
            assert!(message.contains("invalid token"));
        }
        other => panic!("Expected Error::Api, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_business_error_populates_request_record_error() {
    let subscriber =
        tracing_subscriber::Registry::default().with(wecom_transport::telemetry::TraceLayer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = MockServer::start().await;
    let transport = Transport::from(HttpTransportBackend::default());

    Mock::given(method("POST"))
        .and(path("/cgi-bin/biz-err-capture"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(http_api_error_response(40001, "invalid token")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let scope = CaptureScope::new();
    let snaps_arc = collect_spans(&scope);
    let _enter = scope.span().enter();

    let err = transport
        .invoke(ep(&server.uri(), "/cgi-bin/biz-err-capture"), json!({}))
        .await
        .unwrap_err();

    drop(_enter);

    // API 错误应正确返回
    assert!(
        matches!(&err, Error::Api { code, .. } if *code == Some(40001)),
        "expected Error::Api(code=40001), got: {err:?}"
    );

    let snaps = std::mem::take(&mut *snaps_arc.lock().unwrap());
    assert_eq!(
        snaps.len(),
        1,
        "expected 1 captured span, got {}",
        snaps.len()
    );

    let snap = &snaps[0];
    assert!(
        snap.error.is_some(),
        "request_record.error should be Some, but was None"
    );

    let error_val = snap.error.as_ref().unwrap();
    assert_json_eq!(
        error_val,
        &json!({
            "error": {
                "code": 40001,
                "message": "invalid token"
            }
        })
    );
}
