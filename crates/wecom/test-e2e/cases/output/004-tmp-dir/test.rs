#[tokio::test]
async fn run() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    setup_discovery_mocks(&server).await;

    Mock::given(method("POST"))
        .and(path("/department/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"tmp-dir-test-content".to_vec())
                .append_header("content-type", "application/octet-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let custom_tmp = tempfile::tempdir().unwrap();

    let buf = SharedBuf::new();
    let client = wecom::Client::builder()
        .home_dir(custom_tmp.path())
        .tmp_dir(custom_tmp.path())
        .transport(build_test_http_transport("test-token", &server.uri()))
        .writable_dirs(vec![custom_tmp.path().to_path_buf()])
        .build()
        .unwrap();

    let result = client
        .run(hr_dept_list_argv(&[]))
        .output(wecom::CliRunOutput::new(buf.clone()))
        .await;
    assert_cli_ok(&result, &buf, "tmp_dir test");

    let v = assert_stdout_json(&buf);
    let file_path = v["file_path"]
        .as_str()
        .expect("DownloadResult should have file_path");
    let file_canonical =
        std::fs::canonicalize(file_path).unwrap_or_else(|_| std::path::PathBuf::from(file_path));
    let tmp_canonical = custom_tmp.path().canonicalize().unwrap();
    assert!(
        file_canonical.starts_with(&tmp_canonical),
        "file should be under custom tmp_dir\n  file: {}\n  tmp:  {}",
        file_canonical.display(),
        tmp_canonical.display(),
    );
}
