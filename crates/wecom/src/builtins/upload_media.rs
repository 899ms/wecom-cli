use serde::Deserialize;
use wecom_transport::RequestOptions;

use crate::client::EndpointKey;
use crate::{Client, Error, Result, fs};

/// Response from a media upload request.
#[derive(Debug, Deserialize)]
pub struct UploadMediaResponse {
    /// The media ID returned by the server.
    pub media_id: String,
    /// Extra fields returned by the server, captured for forward-compatibility.
    #[serde(flatten)]
    pub extra: indexmap::IndexMap<String, serde_json::Value>,
}

#[tracing::instrument(level = "info", name = "media.upload", skip_all)]
pub(crate) async fn upload_media(
    client: &Client,
    fs: &fs::Fs,
    file_path: &str,
    options: &RequestOptions,
) -> Result<UploadMediaResponse> {
    tracing::info!(%file_path, "upload_media begin");

    let part = fs.open_as_multipart_part(file_path).await?;
    let form = reqwest::multipart::Form::new()
        .part("media", part)
        .text("type", "file");

    let response = client
        .transport()
        .invoke(
            client.resolve_builtin_endpoint(EndpointKey::MediaUpload),
            form,
        )
        .with_options(options.clone())
        .await?
        .into_result()
        .map_err(Error::from)
        .inspect_err(|e| tracing::warn!(error = %e, "upload_media failed"))?;

    let response = UploadMediaResponse::deserialize(&response)
        .map_err(|e| {
            Error::Transport(wecom_transport::Error::Parse {
                message: format!("Failed to deserialize 'upload_media' response: {e:#}"),
                endpoint: "utils://upload_media".into(),
                body: Box::new(response),
                source: Some(e),
            })
        })
        .inspect_err(|e| tracing::warn!(error = %e, "deserialize upload_media response failed"))?;

    tracing::info!(media_id = %response.media_id, "upload_media succeeded");
    Ok(response)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：upload_media（媒体上传 HTTP 路径单元测试）
    //!
    //! ### 关键接口
    //! - [upload_media] — HTTP multipart 上传入口
    //! - [UploadMediaResponse] — 上传结果，含 media_id / extra
    //!
    //! ### 关键分支与异常路径
    //! - HTTP 成功 → result 包含 media_id
    //! - 缺 media_id → UploadMediaResponse 反序列化失败，返回 Parse 错误
    //!
    //! ### 上下游交互
    //! - 上游：client.upload_media() 公共 API
    //! - 下游：Client（transport 分发）、Fs（文件读取）

    use std::io::Write;

    use wecom_transport::RequestOptions;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::Client;

    fn make_http_test_client(base_url: &str, tmp: &std::path::Path) -> Client {
        let home = tempfile::TempDir::new().unwrap();
        let transport = wecom_transport::HttpTransportBackend::builder()
            .base_url(base_url)
            .header_sensitive("Authorization", "Bearer test-token", true)
            .build()
            .unwrap();
        Client::builder()
            .home_dir(home.path())
            .cwd(tmp)
            .writable_dir(tmp)
            .transport(transport)
            .build()
            .unwrap()
    }

    /// P0：[upload_media] HTTP 路径成功上传
    /// 条件：wiremock mock /file/upload 端点，返回 media_id
    /// 断言：result.media_id == "HTTP_MEDIA_001"
    #[tokio::test]
    async fn upload_media_http_success() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("photo.jpg");
        let mut f = std::fs::File::create(&test_file).unwrap();
        f.write_all(b"fake-jpeg-bytes").unwrap();
        f.flush().unwrap();
        drop(f);
        let file_path_str = test_file.to_string_lossy().to_string();

        Mock::given(method("POST"))
            .and(path("/file/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": r#"{"media_id":"HTTP_MEDIA_001"}"#,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_http_test_client(&server.uri(), tmp.path());
        let fs = client.default_fs();
        let result = upload_media(&client, &fs, &file_path_str, &RequestOptions::default())
            .await
            .expect("upload_media HTTP should succeed");

        assert_eq!(result.media_id, "HTTP_MEDIA_001");
    }
}
