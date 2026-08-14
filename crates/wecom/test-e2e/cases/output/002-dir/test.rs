#[tokio::test]
async fn run() {
    let server = wiremock::MockServer::start().await;
    setup_discovery_mocks(&server).await;

    setup_method_mock(
        &server,
        "/department/list",
        api_response(&json!({"departments": [{"id": "1"}]})),
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();

    let buf = SharedBuf::new();
    let client = wecom::Client::builder()
        .home_dir(tmp.path())
        .tmp_dir(tmp.path())
        .transport(build_test_http_transport("test-token", &server.uri()))
        .writable_dirs(vec![tmp.path().to_path_buf()])
        .build()
        .unwrap();

    let result = client
        .run(hr_dept_list_argv(&[
            "--output-dir",
            tmp.path().to_str().unwrap(),
        ]))
        .output(wecom::CliRunOutput::new(buf.clone()))
        .await;
    assert_cli_ok(&result, &buf, "output dir");

    let v = assert_download_result(&buf, "application/json");
    let file_path = v["file_path"].as_str().unwrap();
    assert!(
        file_path.contains("hr_department_list"),
        "file_path should contain method path: {file_path}"
    );
}
