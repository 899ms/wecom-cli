use std::collections::{HashMap, HashSet};

use tracing::field::Empty;
use wecom_transport::RequestOptions;

use super::types::Directive;
use crate::{Client, Error, Result, builtins, constants, fs, json_path};

#[tracing::instrument(
    level = "info",
    name = "media_upload",
    skip_all,
    fields(file_count = Empty),
)]
pub async fn process_media_upload(
    client: &Client,
    fs: &fs::Fs,
    data: &mut serde_json::Value,
    directives: &[Directive<'_>],
    options: &RequestOptions,
) -> Result<()> {
    let file_paths: HashSet<&str> = directives
        .iter()
        .filter_map(|d| match d {
            Directive::UploadMedia { file_path, .. } => Some(file_path.as_str()),
            _ => None,
        })
        .collect();

    if file_paths.is_empty() {
        return Ok(());
    }

    tracing::Span::current().record("file_count", file_paths.len());
    tracing::info!(file_count = file_paths.len(), "uploading media files");

    // 先校验全部文件大小，再批量上传（避免部分上传后发现后续文件超限）
    for file_path in &file_paths {
        fs::check_file_size_limit(fs, file_path, constants::MAX_UPLOAD_SIZE).await?;
    }

    let results: HashMap<String, String> =
        futures::future::try_join_all(file_paths.into_iter().map(|file_path| {
            let options = options.clone();
            async move {
                let response = builtins::upload_media(client, fs, file_path, &options).await?;
                Ok::<_, Error>((file_path.to_string(), response.media_id))
            }
        }))
        .await?
        .into_iter()
        .collect();

    for directive in directives {
        let Directive::UploadMedia {
            path,
            file_path,
            with_file_path,
        } = directive
        else {
            continue;
        };
        let Some(media_id) = results.get(file_path) else {
            continue;
        };
        let replacement = if *with_file_path {
            serde_json::json!({
                "media_id": media_id,
                "file_path": file_path,
            })
        } else {
            serde_json::Value::String(media_id.clone())
        };
        json_path::set_value_deep(data, path, replacement);
    }

    Ok(())
}
