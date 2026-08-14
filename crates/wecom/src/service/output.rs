use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result, fs, schema};

// ── DownloadResult ──────────────────────────────────────────

// `DownloadResult` is the single source of truth: the serialized payload
// ([`to_json`]), the `--schema` JSON Schema ([`json_schema`]) and the `--doc`
// TypeScript declaration ([`ts_doc`]) are all derived from it via `schemars`,
// so there is nothing to keep in sync by hand. The doc-comment below is the
// user-visible description, so keep it Chinese and free of dev-facing notes.
/// 文件下载结果。
#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DownloadResult {
    /// 文件保存的绝对路径
    pub file_path: PathBuf,
    /// 文件大小（字节）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 下载文件的 MIME 类型，例如 `image/png`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl DownloadResult {
    /// TypeScript interface name for the built-in download result.
    const TS_NAME: &'static str = "WeComCliDownloadRes";

    /// Serialize a download result for a path and its already-open file handle.
    ///
    /// `size` is obtained via `file.metadata()` (an `fstat` on the open fd), so
    /// no second path lookup is needed and the result is TOCTOU-safe relative
    /// to the file the caller just wrote into. A failed `fstat` yields `None`.
    pub fn to_json(
        file_path: &Path,
        file: &std::fs::File,
        content_type: &str,
    ) -> serde_json::Value {
        let value = Self {
            file_path: file_path.to_path_buf(),
            size: file.metadata().map(|m| m.len()).ok(),
            content_type: Some(content_type.to_owned()),
        };
        serde_json::to_value(value).expect("DownloadResult serialization should never fail")
    }

    /// TypeScript interface declaration for [`DownloadResult`], used by `--doc` output.
    ///
    /// Generated from the Rust struct via `schemars`, so the shape and field
    /// docs stay in sync with [`json_schema`](Self::json_schema) automatically.
    pub(super) fn ts_doc() -> String {
        let (decl, _deps) = schema::schema_to_ts(Self::TS_NAME, &schema::schema_for_type::<Self>());
        decl
    }

    /// JSON Schema for [`DownloadResult`], used by `--schema` output.
    ///
    /// Derived from the Rust struct via `schemars` rather than hand-written.
    pub(super) fn json_schema() -> serde_json::Value {
        serde_json::to_value(schema::schema_for_type::<Self>()).unwrap_or_default()
    }
}

/// Pre-reserved output destination.
///
/// The file is created exclusively (`create_new`) with restrictive permissions
/// (`0o600` on Unix) so that no window exists between the existence check and
/// creation (TOCTOU-safe).  The caller later writes into `file` and the path
/// is reported back to the user.
pub(super) struct OutputFileInfo {
    path: PathBuf,
    file: std::fs::File,
}

impl OutputFileInfo {
    /// Append a single line (with trailing newline) to the output file.
    ///
    /// Used by the paginate loop to write each page as one NDJSON line.
    pub(super) fn write_line(&mut self, line: &str) -> Result<()> {
        writeln!(self.file, "{line}")
            .map_err(|e| Error::io(format!("Failed to write to {}", self.path.display()), e))
            .inspect_err(|e| tracing::error!(error = %e, "write output file failed"))
    }

    /// Return a JSON value describing the file result (for user-facing output).
    ///
    /// Flushes the underlying file first so that the reported `size` is
    /// accurate even when the fd is still open.
    pub(super) fn result_ndjson(&mut self) -> serde_json::Value {
        // Flush to ensure all buffered data reaches the kernel.
        let _ = self.file.flush();
        DownloadResult::to_json(&self.path, &self.file, "application/x-ndjson")
    }
}

/// Atomically reserve (create) an output file at `path`.
///
/// The path is resolved and security-checked inside [`Fs::create_file`],
/// which also handles parent-directory creation, `create_new(true)` atomicity,
/// `0o600` permissions, and TOCTOU-safe fd-based path verification.
pub(super) async fn create_output_file(
    fs: &fs::Fs,
    path: impl AsRef<Path>,
) -> Result<OutputFileInfo> {
    let (resolved, file) = fs
        .create_file(path)
        .await
        .inspect_err(|e| tracing::error!(error = %e, "create output file failed"))?;
    Ok(OutputFileInfo {
        path: resolved,
        file,
    })
}

/// Atomically reserve (create) an output file at `path`, with unique-suffix
/// collision avoidance.
///
/// If a file at `path` already exists, a random alphanumeric suffix is inserted
/// before the extension and retried (up to 1 000 attempts).
///
/// Delegates to [`Fs::create_file_unique`] so the result is TOCTOU-safe
/// and permissions are `0o600` on Unix.
pub(super) async fn create_output_file_unique(
    fs: &fs::Fs,
    path: impl AsRef<Path>,
) -> Result<OutputFileInfo> {
    let (resolved, file) = fs
        .create_file_unique(path)
        .await
        .inspect_err(|e| tracing::error!(error = %e, "create output file failed"))?;
    Ok(OutputFileInfo {
        path: resolved,
        file,
    })
}

/// Write string `data` to the appropriate destination and return a
/// user-facing result string.
///
/// Priority:
/// 1. `output` – write into the pre-reserved file.
/// 2. `output_dir` – atomically write to `<output_dir>/response.rsp` (with
///    unique-suffix collision avoidance and `0o600` permissions).
/// 3. Neither – return `data` verbatim for stdout.
pub(super) async fn handle_json_output(
    fs: &fs::Fs,
    data: serde_json::Value,
    output_file: Option<OutputFileInfo>,
    output_dir: Option<&Path>,
    method_path: &[String],
) -> Result<serde_json::Value> {
    let mut output = if let Some(output_file) = output_file {
        output_file
    } else if let Some(output_dir) = output_dir {
        let filename = format!("{}.json", method_path.join("_"));
        let filename = fs::sanitize_filename(&filename);
        create_output_file_unique(fs, output_dir.join(filename)).await?
    } else {
        return Ok(data);
    };
    // Write into the pre-reserved file and sync to disk.
    output
        .file
        .write_all(data.to_string().as_bytes())
        .map_err(|e| Error::io(format!("Failed to write to {}", output.path.display()), e))
        .inspect_err(|e| tracing::error!(error = %e, "write output file failed"))?;

    output
        .file
        .sync_all()
        .map_err(|e| Error::io(format!("Failed to sync {}", output.path.display()), e))
        .inspect_err(|e| tracing::error!(error = %e, "sync output file failed"))?;

    Ok(DownloadResult::to_json(
        &output.path,
        &output.file,
        "application/json",
    ))
}

/// Stream a binary `response` body to the appropriate destination and return a
/// user-facing result string.
///
/// Both paths use streaming I/O so that large files never need to be buffered
/// entirely in memory.  When the server provides a `Content-Length` header the
/// destination file is pre-allocated (`set_len`) before writing so the
/// filesystem can reserve contiguous space up-front.
///
/// Priority:
/// 1. `output` – stream into the pre-reserved file.
/// 2. `output_dir` / default output dir – stream into a temp file first, then
///    atomically rename into the target directory (TOCTOU-safe, `0o600`).
pub(super) async fn handle_binary_output(
    options: &super::RunOptions<'_>,
    response: wecom_transport::HttpResponse,
    output_file: Option<OutputFileInfo>,
    method_path: &[String],
) -> Result<serde_json::Value> {
    let fs = options.run.fs();
    let output = if let Some(file_info) = output_file {
        file_info
    } else {
        let output_dir = options.output_dir();
        let filename = fs::content_disposition_filename(response.headers())
            .unwrap_or_else(|| format!("{}.bin", method_path.join("_")));
        let filename = fs::sanitize_filename(&filename);
        create_output_file_unique(fs, output_dir.join(filename)).await?
    };

    // Stream directly into the pre-reserved file via tokio async I/O.
    let mut file = tokio::fs::File::from_std(output.file);

    // Pre-allocate disk space when the server tells us the size
    // (prefers Content-Range total over Content-Length).
    if let Some(len) = response.total_length() {
        file.set_len(len)
            .await
            .map_err(|e| {
                Error::io(
                    format!(
                        "Failed to pre-allocate {len} bytes for {}",
                        output.path.display()
                    ),
                    e,
                )
            })
            .inspect_err(|e| tracing::error!(error = %e, "pre-allocate file failed"))?;
    }

    let content_type = response
        .headers()
        .get("Content-Type")
        .and_then(|s| s.to_str().ok())
        .map(str::to_owned);

    fs::stream_to_file(&mut file, &output.path, response)
        .await
        .inspect_err(|e| tracing::error!(error = %e, "stream to file failed"))?;

    // Recover the underlying `std::fs::File` so we can fstat it without a
    // second path-based lookup.  `stream_to_file` already called `sync_all`.
    let std_file = file.into_std().await;

    Ok(DownloadResult::to_json(
        &output.path,
        &std_file,
        content_type
            .as_deref()
            .unwrap_or("application/octet-stream"),
    ))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：output（输出处理与文件管理）
    //!
    //! ### 关键接口
    //! - [create_output_file] — 原子创建输出文件（TOCTOU-safe）
    //! - [create_output_file_unique] — 带唯一后缀冲突避免的文件创建
    //! - [handle_json_output] — 按优先级写入 JSON 数据（output > output_dir > stdout）
    //! - [handle_binary_output] — 流式写入二进制响应到目标路径
    //! - [DownloadResult::to_json] — 生成下载结果 JSON
    //! - [DownloadResult::json_schema] / [DownloadResult::ts_doc] — 由 schemars 从结构体派生 schema / TS
    //!
    //! ### 关键分支与异常路径
    //! - 文件已存在 → create_output_file 返回 Err（原子创建冲突）
    //! - 空文件 → DownloadResult 的 size 为 0
    //! - 无 output_file/output_dir → handle_json_output 原样返回数据
    //! - 同名文件冲突 → create_output_file_unique 追加随机后缀
    //! - schema / TS 均由 #[derive(schemars::JsonSchema)] 派生，描述来自结构体 doc-comment
    //!
    //! ### 上下游交互
    //! - 上游：[execute::execute_and_output] 调用 handle_json_output/handle_binary_output
    //! - 下游：依赖 [Fs]（原子写入、创建文件）、tokio 异步 I/O；schema/TS 经 [crate::schema] 派生

    use std::fs;

    use assert_json_diff::assert_json_include;
    use tempfile::TempDir;

    use super::*;
    use crate::fs as crate_fs;

    fn test_fs(dir: &Path) -> crate_fs::Fs {
        crate_fs::Fs::new(dir)
    }

    // ── DownloadResult ──

    /// P0：[DownloadResult::to_json] 对已存在文件生成正确结果
    /// 条件：打开已写入 "hello"（5 字节）的文件句柄，content_type 为 "image/png"
    /// 断言：content_type 为 "image/png"，size 为 5，file_path 包含文件名
    #[test]
    fn download_result_to_json_existing_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.bin");
        fs::write(&file_path, b"hello").unwrap();
        let file = fs::File::open(&file_path).unwrap();

        let val = DownloadResult::to_json(&file_path, &file, "image/png");
        assert_json_diff::assert_json_eq!(val["content_type"], serde_json::json!("image/png"));
        assert_json_diff::assert_json_eq!(val["size"], serde_json::json!(5));
        assert!(val["file_path"].as_str().unwrap().contains("test.bin"));
    }

    /// P1：[DownloadResult::to_json] 空文件的处理
    /// 条件：新建空文件并传入其句柄
    /// 断言：size 为 0，content_type 原样回显
    #[test]
    fn download_result_to_json_empty_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("empty.bin");
        let file = fs::File::create(&file_path).unwrap();

        let val = DownloadResult::to_json(&file_path, &file, "application/octet-stream");
        assert_json_diff::assert_json_eq!(
            val["content_type"],
            serde_json::json!("application/octet-stream")
        );
        assert_json_diff::assert_json_eq!(val["size"], serde_json::json!(0));
    }

    /// P1：[DownloadResult::json_schema] 由 schemars 派生，描述为中文且字段齐全
    /// 条件：调用派生生成的 json_schema（单一定义来源）
    /// 断言：type=object、中文描述、size 为 integer；仅 file_path 必填（size/content_type 为 Option）
    #[test]
    fn download_result_schema_is_chinese_and_derived() {
        let schema = DownloadResult::json_schema();
        assert_json_include!(
            actual: schema.clone(),
            expected: serde_json::json!({
                "type": "object",
                "description": "文件下载结果。",
                "properties": {
                    "content_type": { "type": "string", "description": "下载文件的 MIME 类型，例如 `image/png`" },
                    "file_path": { "type": "string", "description": "文件保存的绝对路径" },
                    "size": { "type": "integer", "description": "文件大小（字节）" }
                }
            })
        );
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert!(required.iter().any(|r| r == "file_path"));
        for optional in ["size", "content_type"] {
            assert!(
                !required.iter().any(|r| r == optional),
                "{optional} should be optional"
            );
        }
    }

    /// P1：[DownloadResult::ts_doc] 生成中文 JSDoc 的 TS 接口且不泄漏开发者说明
    /// 条件：调用由 schemars 派生的 ts_doc
    /// 断言：接口名为 WeComCliDownloadRes、含中文描述、size/content_type 为可选字段（带 ?）
    #[test]
    fn download_result_ts_doc_is_chinese() {
        let ts = DownloadResult::ts_doc();
        assert!(ts.contains("interface WeComCliDownloadRes"));
        assert!(ts.contains("/** 文件下载结果。 */"));
        assert!(ts.contains("file_path: string;"));
        assert!(ts.contains("content_type?: string;"));
        assert!(ts.contains("size?: number;"));
        assert!(ts.contains("下载文件的 MIME 类型，例如 `image/png`"));
        // 用户可见的 TS 不应混入开发者实现说明
        assert!(!ts.contains("single source of truth"));
    }

    // ── reserve_output_file ──

    /// P0：[create_output_file] 创建新文件
    /// 条件：目标路径不存在
    /// 断言：文件创建成功，路径存在
    #[tokio::test]
    async fn reserve_output_file_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());
        let path = tmp.path().join("output.json");

        let info = create_output_file(&fs_handle, &path).await.unwrap();
        assert!(info.path.exists());
    }

    /// P1：[create_output_file] 在文件已存在时返回错误
    /// 条件：目标文件已预先创建
    /// 断言：返回 Err（原子创建冲突）
    #[tokio::test]
    async fn reserve_output_file_fails_if_exists() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());
        let path = tmp.path().join("output.json");
        fs::write(&path, "").unwrap();

        let result = create_output_file(&fs_handle, &path).await;
        assert!(result.is_err());
    }

    /// P1：[create_output_file] 自动创建父目录
    /// 条件：目标路径包含多层不存在的父目录 a/b/
    /// 断言：文件创建成功，路径存在
    #[tokio::test]
    async fn reserve_output_file_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());
        let path = tmp.path().join("a").join("b").join("output.json");

        let info = create_output_file(&fs_handle, &path).await.unwrap();
        assert!(info.path.exists());
    }

    // ── OutputFileInfo::write_line / result_json ──

    /// P0：[OutputFileInfo::write_line] 和 [OutputFileInfo::result_ndjson] 功能
    /// 条件：创建文件后写入两行 NDJSON 数据
    /// 断言：result_ndjson 返回 content_type 为 ndjson，size > 0
    #[tokio::test]
    async fn output_file_info_write_line_and_result() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());
        let path = tmp.path().join("test.ndjson");

        let mut info = create_output_file(&fs_handle, &path).await.unwrap();
        info.write_line(r#"{"page":1}"#).unwrap();
        info.write_line(r#"{"page":2}"#).unwrap();

        let result = info.result_ndjson();
        assert_json_diff::assert_json_eq!(
            result["content_type"],
            serde_json::json!("application/x-ndjson")
        );
        let size = result["size"].as_u64().unwrap();
        assert!(size > 0, "file size should be > 0");
    }

    // ── create_output_file_unique ──

    /// P0：[create_output_file_unique] 创建新文件
    /// 条件：目标路径无冲突
    /// 断言：文件创建成功，文件名以 "test.ndjson" 结尾
    #[tokio::test]
    async fn create_output_file_unique_creates_file() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());

        let info = create_output_file_unique(&fs_handle, tmp.path().join("test.ndjson"))
            .await
            .unwrap();
        assert!(info.path.exists());
        assert!(info.path.to_string_lossy().ends_with("test.ndjson"));
    }

    /// P1：[create_output_file_unique] 通过唯一后缀避免文件名冲突
    /// 条件：同一路径调用两次，第二次文件已存在
    /// 断言：两次返回不同路径，第二个路径仍包含原文件名且以 .ndjson 结尾
    #[tokio::test]
    async fn create_output_file_unique_avoids_collision() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());

        let info1 = create_output_file_unique(&fs_handle, tmp.path().join("data.ndjson"))
            .await
            .unwrap();
        let info2 = create_output_file_unique(&fs_handle, tmp.path().join("data.ndjson"))
            .await
            .unwrap();
        assert_ne!(info1.path, info2.path);
        assert!(info2.path.to_string_lossy().contains("data."));
        assert!(info2.path.to_string_lossy().ends_with(".ndjson"));
    }

    // ── handle_json_output ──

    /// P0：[handle_json_output] 在无输出文件/目录时原样返回数据
    /// 条件：output_file 和 output_dir 均为 None
    /// 断言：返回原始 JSON 数据不变
    #[tokio::test]
    async fn handle_json_output_no_file_no_dir_returns_data() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());

        let data = serde_json::json!({"key": "value"});
        let result = handle_json_output(&fs_handle, data.clone(), None, None, &["method".into()])
            .await
            .unwrap();
        assert_json_diff::assert_json_eq!(result, data);
    }

    /// P0：[handle_json_output] 写入到预分配的输出文件
    /// 条件：提供了预分配的 output_file
    /// 断言：返回 DownloadResult 结构，文件中包含写入的内容
    #[tokio::test]
    async fn handle_json_output_to_file() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());
        let path = tmp.path().join("out.json");
        let output_file = create_output_file(&fs_handle, &path).await.unwrap();

        let data = serde_json::json!({"foo": "bar"});
        let result = handle_json_output(&fs_handle, data, Some(output_file), None, &["m".into()])
            .await
            .unwrap();

        assert_json_diff::assert_json_eq!(
            result["content_type"],
            serde_json::json!("application/json")
        );
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("bar"));
    }

    /// P0：handle_json_output 写入到 output_dir 目录
    /// 条件：output_file 为 None，提供 output_dir
    /// 断言：返回 DownloadResult，content_type 为 application/json
    #[tokio::test]
    async fn handle_json_output_to_dir() {
        let tmp = TempDir::new().unwrap();
        let fs_handle = test_fs(tmp.path());

        let data = serde_json::json!({"baz": 42});
        let result = handle_json_output(
            &fs_handle,
            data,
            None,
            Some(tmp.path()),
            &["svc".into(), "method".into()],
        )
        .await
        .unwrap();

        assert_json_diff::assert_json_eq!(
            result["content_type"],
            serde_json::json!("application/json")
        );
    }
}
