use std::path::PathBuf;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use serde_with::skip_serializing_none;

use crate::client::CliRun;

/// Options for a programmatic API call.
///
/// Holds a reference to the [`CliRun`] that created it so that per-run
/// path overrides (home_dir, tmp_dir, etc.) are automatically respected
/// by downstream code without duplicating fields.
pub struct RunOptions<'r> {
    /// Back-reference to the originating [`CliRun`].
    ///
    /// Downstream code can call `run.get_request_storage_dir()`,
    /// `run.get_cache_dir()`, etc. to obtain paths that honour per-run
    /// overrides.
    pub run: &'r CliRun<'r>,
    /// Request payload (default `{}`).
    pub payload: Value,
    /// Number of pages to fetch (Some enables auto-pagination).
    pub page_count: Option<u32>,
    /// Delay between pages in milliseconds.
    pub page_delay_ms: u64,
    /// Output file path (for binary downloads).
    pub output_path: Option<PathBuf>,
    /// Output directory (for binary downloads / file-save directives).
    pub output_dir: Option<PathBuf>,
}

impl<'r> RunOptions<'r> {
    /// Create a new `RunOptions` with sensible defaults, bound to the
    /// given [`CliRun`].
    pub fn new(run: &'r CliRun<'r>) -> Self {
        Self {
            run,
            payload: serde_json::json!({}),
            page_count: None,
            page_delay_ms: 0,
            output_path: None,
            output_dir: None,
        }
    }

    /// Effective output directory: explicit `output_dir` override, or the
    /// request storage directory derived from the [`CliRun`].
    ///
    /// Centralises the fallback logic shared by `handle_binary_output` and
    /// `process_file_save`.
    pub fn output_dir(&self) -> PathBuf {
        self.output_dir
            .clone()
            .unwrap_or_else(|| self.run.get_request_storage_dir())
    }
}

/// Structured schema information for a service.
///
/// Returned by [`ServiceHandle::schema()`].
#[derive(Debug, Clone, Serialize)]
pub struct ServiceSchemaInfo {
    /// Service name.
    pub name: String,
    /// Service description (if any).
    pub description: Option<String>,
    /// Skills provided by the backend for this service.
    pub skills: Vec<String>,
    /// Flattened list of all methods in this service.
    pub methods: Vec<MethodSummary>,
}

/// Summary of a single method within a service.
#[derive(Debug, Clone, Serialize)]
pub struct MethodSummary {
    /// Method name (last path segment).
    pub name: String,
    /// Method description (if any).
    pub description: Option<String>,
}

/// Structured schema information for a method.
///
/// Returned by [`MethodHandle::schema()`] — contains the method path,
/// description, request/response references, and resolved schema definitions.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize)]
pub struct MethodSchemaInfo {
    /// Dot-joined method path, e.g. `"department.create"`.
    pub method: String,
    /// Method description (if any).
    pub description: Option<String>,
    /// Request schema reference (if any).
    pub request: Option<Value>,
    /// Response schema reference.
    pub response: Value,
    /// Resolved schema definitions referenced by request/response.
    pub schemas: Value,
}

/// A single HTTP request that would be sent during a dry-run.
///
/// Returned by [`MethodHandle::preview()`] — one entry per request
/// (media uploads produce additional entries before the main request).
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize)]
pub struct RequestInfo {
    /// HTTP method (e.g. "POST").
    pub method: String,
    /// Full URL.
    pub url: String,
    /// Request headers (sensitive values masked).
    pub headers: IndexMap<String, String>,
    /// JSON payload (for non-multipart requests).
    pub payload: Option<Value>,
    /// Multipart form parts (for multipart requests).
    pub multipart: Option<Vec<MultipartPart>>,
}

/// A single part in a multipart form request.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MultipartPart {
    /// A file upload part.
    File {
        /// Form field name.
        name: String,
        /// Local file path.
        file: String,
    },
    /// A text field part.
    Text {
        /// Form field name.
        name: String,
        /// Field value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：service 类型定义（RunOptions、服务/方法 Schema、请求预览等数据模型）
    //!
    //! ### 关键接口
    //! - [RunOptions::new] — 创建带默认值的调用选项
    //! - [RunOptions::output_dir] — 获取有效的输出目录（含回退逻辑）
    //! - [ServiceSchemaInfo] / [MethodSchemaInfo] — Schema 信息序列化
    //! - [RequestInfo] / [MultipartPart] — 请求预览和 multipart 序列化
    //!
    //! ### 关键分支与异常路径
    //! - output_dir 显式设置 → 返回显式值
    //! - output_dir 未设置 → 回退到 request_storage_dir
    //! - tmp_dir 覆盖 → 路径随覆盖变化
    //! - multipart=None → skip_serializing_none 跳过
    //!
    //! ### 上下游交互
    //! - 上游：service::execute、service::output 等使用 RunOptions
    //! - 下游：序列化为 JSON 输出给 CLI 用户或上传接口

    use assert_json_diff::assert_json_eq;
    use serde_json::json;

    use super::*;
    use crate::Client;

    /// Build an isolated [Client] for unit tests.
    fn build_isolated_client() -> Client {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Client::builder().home_dir(&dir).cwd(&dir).build().unwrap()
    }

    /// P0：RunOptions::new 默认值正确
    /// 条件：使用 RunOptions::new(run) 创建
    /// 断言：payload 为空对象，page_count=None，delay=0，output 路径均为 None
    #[test]
    fn call_options_default_values() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        let opts = RunOptions::new(&run);
        assert_json_eq!(opts.payload, json!({}));
        assert_eq!(opts.page_count, None);
        assert_eq!(opts.page_delay_ms, 0);
        assert!(opts.output_path.is_none());
        assert!(opts.output_dir.is_none());
    }

    /// P0：RunOptions 通过 run 引用可访问 headers
    /// 条件：CliRun 设置了自定义 header "x-inherit"
    /// 断言：opts.run.get_headers() 包含 "x-inherit"
    #[test]
    fn run_options_accesses_headers_via_run() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).header("x-inherit", "yes");
        let opts = RunOptions::new(&run);
        assert!(opts.run.get_headers().contains_key("x-inherit"));
    }

    /// P0：RunOptions 自定义值可正确设置和读取
    /// 条件：手动设置所有字段为非默认值
    /// 断言：各字段返回值与设置值一致
    #[test]
    fn call_options_custom_values() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        let opts = RunOptions {
            run: &run,
            payload: json!({"key": "value"}),
            page_count: Some(3),
            page_delay_ms: 200,
            output_path: Some(PathBuf::from("/tmp/out.json")),
            output_dir: Some(PathBuf::from("/tmp/outdir")),
        };
        assert_json_eq!(opts.payload["key"], serde_json::json!("value"));
        assert_eq!(opts.page_count, Some(3));
        assert_eq!(opts.page_delay_ms, 200);
        assert_eq!(opts.output_path.unwrap(), PathBuf::from("/tmp/out.json"));
        assert_eq!(opts.output_dir.unwrap(), PathBuf::from("/tmp/outdir"));
    }

    /// P1：RunOptions 通过 run 引用可访问路径覆盖
    /// 条件：CliRun 设置了 tmp_dir 覆盖
    /// 断言：opts.run.get_request_storage_dir() 返回覆盖后的路径
    #[test]
    fn run_options_accesses_path_overrides_via_run() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).tmp_dir("/custom/tmp");
        let opts = RunOptions::new(&run);
        assert_eq!(
            opts.run.get_request_storage_dir(),
            std::path::PathBuf::from("/custom/tmp/requests")
        );
    }

    /// P1：RunOptions 无 tmp_dir 覆盖时回退到 client 默认值
    /// 条件：CliRun 未设置 tmp_dir 覆盖
    /// 断言：run.get_request_storage_dir() 返回 client.request_storage_dir()
    #[test]
    fn run_options_falls_back_to_client_without_override() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        let opts = RunOptions::new(&run);
        assert_eq!(
            opts.run.get_request_storage_dir(),
            client.request_storage_dir()
        );
    }

    // ── output_dir ──

    /// P0：[RunOptions::output_dir] 在 output_dir 为 Some 时返回显式目录
    /// 条件：output_dir 设置为 "/explicit/dir"
    /// 断言：返回 PathBuf("/explicit/dir")
    #[test]
    fn effective_output_dir_returns_explicit_when_set() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        let mut opts = RunOptions::new(&run);
        opts.output_dir = Some(PathBuf::from("/explicit/dir"));
        assert_eq!(opts.output_dir(), PathBuf::from("/explicit/dir"));
    }

    /// P0：[RunOptions::output_dir] 在 output_dir 为 None 时回退到 request_storage_dir
    /// 条件：output_dir 未设置
    /// 断言：返回 run.get_request_storage_dir()
    #[test]
    fn effective_output_dir_falls_back_to_request_storage_dir() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        let opts = RunOptions::new(&run);
        assert_eq!(opts.output_dir(), run.get_request_storage_dir());
    }

    /// P1：[RunOptions::output_dir] 在 tmp_dir 覆盖时使用覆盖后的路径
    /// 条件：CliRun 设置了 tmp_dir 覆盖，output_dir 未设置
    /// 断言：返回覆盖后的 request_storage_dir
    #[test]
    fn effective_output_dir_respects_tmp_dir_override() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).tmp_dir("/custom/tmp");
        let opts = RunOptions::new(&run);
        assert_eq!(opts.output_dir(), PathBuf::from("/custom/tmp/requests"));
    }

    /// P0：ServiceSchemaInfo 可正确序列化为 JSON
    /// 条件：创建包含 name、description 和 methods 的 ServiceSchemaInfo
    /// 断言：JSON 中各字段值正确
    #[test]
    fn service_schema_info_serializes() {
        let info = ServiceSchemaInfo {
            name: "hr".to_string(),
            description: Some("Human Resources".to_string()),
            skills: vec!["hr-skill".to_string()],
            methods: vec![MethodSummary {
                name: "hr.department.list".to_string(),
                description: Some("List departments".to_string()),
            }],
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_json_eq!(json["name"], serde_json::json!("hr"));
        assert_json_eq!(json["description"], serde_json::json!("Human Resources"));
        assert_json_eq!(json["skills"][0], serde_json::json!("hr-skill"));
        assert_json_eq!(
            json["methods"][0]["name"],
            serde_json::json!("hr.department.list")
        );
    }

    /// P0：MethodSchemaInfo 带可选 request 字段时序列化正确
    /// 条件：创建包含 method、description、request（Some）、response 的 MethodSchemaInfo
    /// 断言：JSON 中 request 为对象类型
    #[test]
    fn method_schema_info_serializes_with_optional_request() {
        let info = MethodSchemaInfo {
            method: "department.create".to_string(),
            description: Some("Create dept".to_string()),
            request: Some(json!({"$ref": "CreateReq"})),
            response: json!({"$ref": "CreateRes"}),
            schemas: json!({}),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_json_eq!(json["method"], serde_json::json!("department.create"));
        assert!(json["request"].is_object());
    }

    /// RequestInfo 序列化正确，multipart=None 时被跳过
    /// 条件：创建带 method、url、headers、payload 的 RequestInfo，multipart 为 None
    /// 断言：JSON 中不包含 multipart 字段（skip_serializing_none）
    /// RequestInfo 可正确序列化为 JSON
    /// 条件：构造含 method/url/headers/payload 的 RequestInfo
    /// 断言：JSON 中 method/url 匹配，multipart=None 时被跳过
    #[test]
    fn request_info_serializes() {
        let info = RequestInfo {
            method: "POST".to_string(),
            url: "https://api.example.com/dept/list".to_string(),
            headers: IndexMap::from([("Authorization".to_string(), "***".to_string())]),
            payload: Some(json!({"id": "1"})),
            multipart: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_json_eq!(json["method"], serde_json::json!("POST"));
        assert_json_eq!(
            json["url"],
            serde_json::json!("https://api.example.com/dept/list")
        );
        assert!(json.get("multipart").is_none()); // skipped when None
    }

    /// P1：MultipartPart 枚举使用 tag 类型正确序列化
    /// 条件：分别创建 File 和 Text 两种 MultipartPart 变体
    /// 断言：File 序列化为 type:"file"，Text 序列化为 type:"text"
    #[test]
    fn multipart_part_serializes_with_tag() {
        let file_part = MultipartPart::File {
            name: "media".to_string(),
            file: "/tmp/upload.txt".to_string(),
        };
        let text_part = MultipartPart::Text {
            name: "type".to_string(),
            value: "file".to_string(),
        };
        let file_json = serde_json::to_value(&file_part).unwrap();
        assert_json_eq!(file_json["type"], serde_json::json!("file"));
        assert_json_eq!(file_json["name"], serde_json::json!("media"));

        let text_json = serde_json::to_value(&text_part).unwrap();
        assert_json_eq!(text_json["type"], serde_json::json!("text"));
        assert_json_eq!(text_json["value"], serde_json::json!("file"));
    }
}
