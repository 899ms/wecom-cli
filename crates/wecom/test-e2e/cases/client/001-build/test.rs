#[test]
fn run() {
    let home = leaked_tempdir();
    let tmp = leaked_tempdir();

    let transport = wecom_transport::HttpTransportBackend::builder()
        .base_url("https://custom.api.com")
        .header_sensitive("Authorization", "Bearer my-token", true)
        .build()
        .unwrap();

    let client = wecom::Client::builder()
        .home_dir(&home)
        .tmp_dir(&tmp)
        .transport(transport)
        .build()
        .unwrap();

    assert_eq!(client.home_dir(), home.as_path());
    assert_eq!(client.tmp_dir(), tmp.as_path());
    // Transport should have Authorization header
    assert!(client.transport().headers().contains_key("authorization"));
}
