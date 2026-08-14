/// P0：Builder 注入 PathResolver，虚拟路径被映射到物理路径进行文件写入
/// 条件：ClientBuilder::path_resolver 映射 "virtual://<rest>" → "<physical_dir>/<rest>"
/// 断言：DownloadResult.file_path 为映射后的物理路径，文件创建在物理目录下
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
    let physical_dir = tmp.path().to_path_buf();

    let resolver: PathResolver = Arc::new(move |p: &Path| {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix("virtual://") {
            Ok(physical_dir.join(rest))
        } else {
            Ok(p.to_path_buf())
        }
    });

    let buf = SharedBuf::new();
    let client = wecom::Client::builder()
        .home_dir(tmp.path())
        .tmp_dir(tmp.path())
        .transport(build_test_http_transport("test-token", &server.uri()))
        .writable_dirs(vec![tmp.path().to_path_buf()])
        .path_resolver(resolver)
        .build()
        .unwrap();

    let result = client
        .run(hr_dept_list_argv(&["--output", "virtual://out.json"]))
        .output(wecom::CliRunOutput::new(buf.clone()))
        .await;
    assert_cli_ok(&result, &buf, "path resolver via builder");

    // CLI stdout should be DownloadResult with mapped physical path
    let v = assert_download_result(&buf, "application/json");
    let file_path = v["file_path"].as_str().unwrap();
    assert!(
        !file_path.starts_with("virtual://"),
        "file_path should be a physical path, got: {file_path}"
    );
    assert!(
        file_path.contains("out.json"),
        "file_path should contain out.json, got: {file_path}"
    );

    // FS: file exists at the mapped physical path
    let expected = tmp.path().join("out.json");
    let content = assert_file_exists(&expected);
    let file_v: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(file_v["departments"].is_array());
}
