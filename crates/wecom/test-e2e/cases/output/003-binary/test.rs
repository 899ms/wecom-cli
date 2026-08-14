#[tokio::test]
async fn run() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    setup_discovery_mocks(&server).await;

    let binary_content = b"fake binary content for testing";

    Mock::given(method("POST"))
        .and(path("/department/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(binary_content.to_vec())
                .append_header("content-type", "application/octet-stream")
                .append_header(
                    "content-disposition",
                    r#"attachment; filename="report.xlsx""#,
                ),
        )
        .expect(1)
        .mount(&server)
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
    assert_cli_ok(&result, &buf, "binary download");

    let v = assert_download_result(&buf, "application/octet-stream");
    let file_path_str = v["file_path"].as_str().unwrap();
    assert!(
        file_path_str.contains("report.xlsx"),
        "file_path should contain Content-Disposition filename: {file_path_str}"
    );
    assert_eq!(v["size"], binary_content.len() as u64);

    // FS: verify file content
    #[allow(clippy::disallowed_methods)]
    let saved = std::fs::read(file_path_str).unwrap();
    assert_eq!(saved, binary_content);
}
