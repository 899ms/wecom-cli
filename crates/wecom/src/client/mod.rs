mod builder;
mod catalog;
mod custom_command;
mod invoke;
mod run;
mod upload;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use builder::ClientBuilder;
pub use catalog::{EndpointCatalog, EndpointKey, PayloadStringReq};
pub use custom_command::CustomCommand;
pub use invoke::ClientInvokeRequest;
pub use run::{CliRun, CliRunOutput, Writer};
pub use upload::ClientUploadMediaRequest;
use wecom_transport::RequestOptions;

use crate::helpers::HelperRegistry;
use crate::{Error, Result, fs, registry, service};

/// The main entry point for the wecom library.
///
/// `Client` owns all runtime state: HTTP client, caches, paths, and
/// configuration. Create one via [`Client::builder()`] or [`Client::from_defaults()`].
pub struct Client {
    // -- file-system default options --
    cwd: PathBuf,
    readable_dirs: Option<Vec<PathBuf>>,
    writable_dirs: Option<Vec<PathBuf>>,
    path_resolver: Option<fs::PathResolver>,

    // -- paths --
    home_dir: PathBuf,
    tmp_dir: PathBuf,

    // -- cli name --
    /// 二进制名（命令名），用于 `--version` / `--help` / `--doc` 输出。
    bin_name: String,

    // -- networking --
    transport: wecom_transport::Transport,

    // -- endpoint catalog --
    /// 内置（非 schema 驱动）endpoint 的集中配置目录。
    endpoints: Arc<EndpointCatalog>,

    // -- runtime (internal) --
    service_cache: tokio::sync::Mutex<registry::ServiceCache>,
    helper_registry: HelperRegistry,

    // -- custom commands --
    custom_commands: Vec<CustomCommand>,
}

// -- Client impl --

impl Client {
    // -- constructors --

    /// Create a [`ClientBuilder`] starting from defaults.
    ///
    /// This is the preferred way to construct a [`Client`] when you need
    /// fine-grained control over configuration.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Convenience constructor — equivalent to `Self::builder().build()`.
    pub fn from_defaults() -> Result<Self> {
        Self::builder().build()
    }

    // -- file-system --

    /// Default working directory configured at construction time.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Readable root directories, if any.
    pub fn readable_dirs(&self) -> Option<&[PathBuf]> {
        self.readable_dirs.as_deref()
    }

    /// Writable root directories, if any.
    pub fn writable_dirs(&self) -> Option<&[PathBuf]> {
        self.writable_dirs.as_deref()
    }

    /// Build a sandboxed [`fs::Fs`] from the client's default `cwd` and
    /// readable / writable root lists.
    ///
    /// This is the canonical way to obtain an [`fs::Fs`] that honors the
    /// client's configured sandbox.  Both [`Client::run`] and the
    /// programmatic upload builder ([`ClientUploadMediaRequest`]) use this
    /// so all entry points share identical path-validation semantics.
    pub fn default_fs(&self) -> fs::Fs {
        let readable: Option<Vec<&Path>> = self
            .readable_dirs
            .as_ref()
            .map(|dirs| dirs.iter().map(|p| p.as_path()).collect());
        let writable: Option<Vec<&Path>> = self
            .writable_dirs
            .as_ref()
            .map(|dirs| dirs.iter().map(|p| p.as_path()).collect());
        let mut fs =
            fs::Fs::new_with_permissions(&self.cwd, readable.as_deref(), writable.as_deref());
        if let Some(resolver) = &self.path_resolver {
            fs = fs.with_resolver(Arc::clone(resolver));
        }
        fs
    }

    /// Root configuration directory (default `~/.config/wecom`).
    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    /// Directory used for on-disk caches (`<home_dir>/cache`).
    ///
    /// Derived from [`home_dir`](Self::home_dir); cannot be set independently.
    pub fn cache_dir(&self) -> PathBuf {
        self.home_dir.join("cache")
    }

    /// Temporary directory for scratch files (default `$TMPDIR/wecom`).
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// The binary name (command name) used in `--version` / `--help` /
    /// `--doc` output.
    pub fn bin_name(&self) -> &str {
        &self.bin_name
    }

    /// Default directory where HTTP response files are stored (`<tmp_dir>/requests`).
    ///
    /// Derived from [`tmp_dir`](Self::tmp_dir); cannot be set independently.
    pub fn request_storage_dir(&self) -> PathBuf {
        self.tmp_dir.join("requests")
    }

    // -- networking --

    /// Return the [`wecom_transport::Transport`] handle for the client.
    pub fn transport(&self) -> &wecom_transport::Transport {
        &self.transport
    }

    /// Return the mutable [`wecom_transport::Transport`] handle for the client.
    pub fn transport_mut(&mut self) -> &mut wecom_transport::Transport {
        &mut self.transport
    }

    /// Derive an [`wecom_transport::Endpoint`] using this client's configuration.
    ///
    /// base_url is `None` — the transport fills its
    /// defaults at execution time. Use [`MethodHandle::endpoint`] for
    /// per-service overrides.
    ///
    /// The returned endpoint carries HTTP addressing.
    ///
    /// # Example
    /// ```ignore
    /// // Service discovery endpoint:
    /// client.endpoint("/service/discovery");
    /// ```
    pub fn endpoint(&self, path: impl Into<String>) -> wecom_transport::Endpoint {
        wecom_transport::Endpoint::new().with(wecom_transport::HttpEndpoint::new(path))
    }

    // -- runtime (internal) --

    /// Returns a reference to the shared [`HelperRegistry`].
    pub fn helper_registry(&self) -> &HelperRegistry {
        &self.helper_registry
    }

    /// Returns the custom commands registered via [`ClientBuilder::command`].
    pub fn custom_commands(&self) -> &[CustomCommand] {
        &self.custom_commands
    }

    // -- programmatic service API --

    /// List all available services from the registry catalog.
    ///
    /// Results are cached after the first fetch; subsequent calls return
    /// the cached list.
    pub async fn list_services(&self) -> Result<Vec<registry::ServiceInfo>> {
        self.list_services_with_options(&RequestOptions::default())
            .await
    }

    /// Get a handle to a specific service by name.
    ///
    /// Fetches the service description (with caching) and returns a
    /// [`service::ServiceHandle`] whose methods are all synchronous.
    ///
    /// # Example
    /// ```ignore
    /// let svc = client.service("contact").await?;
    /// let method = svc.method(&["users", "list"])?;
    /// method.invoke(serde_json::json!({})).await?;
    /// ```
    pub async fn service(&self, name: &str) -> Result<service::ServiceHandle<'_>> {
        self.service_with_options(name, &RequestOptions::default())
            .await
    }

    /// Get a [`MethodHandle`] directly by path, e.g. `&["contact", "users", "list"]`.
    ///
    /// This is a convenience shortcut for:
    /// ```ignore
    /// client.service("contact").await?.method(&["users", "list"])?
    /// ```
    ///
    /// The first element is the service name; the remaining elements are the
    /// method path passed to [`service::ServiceHandle::method`].
    ///
    /// # Errors
    /// Returns [`Error::Validation`] if the path has fewer than two elements,
    /// the service is not found, or the method path does not exist.
    ///
    /// # Example
    /// ```ignore
    /// let method = client.method(&["contact", "users", "list"]).await?;
    /// method.invoke(serde_json::json!({})).await?;
    /// ```
    pub async fn method(&self, path: &[&str]) -> Result<service::MethodHandle<'_>> {
        self.method_with_options(path, &RequestOptions::default())
            .await
    }

    // -- internal helpers --

    /// [`list_services`](Self::list_services) 的带请求 options 变体：
    /// discovery 请求会以 `options`（叠加在 transport 默认之上）发出。
    pub(crate) async fn list_services_with_options(
        &self,
        options: &RequestOptions,
    ) -> Result<Vec<registry::ServiceInfo>> {
        let catalog = self
            .service_cache
            .lock()
            .await
            .get_or_fetch_catalog(self, options)
            .await?;
        Ok(catalog.items.clone())
    }

    /// [`service`](Self::service) 的带请求 options 变体：schema 拉取请求
    /// 会以 `options`（叠加在 transport 默认之上）发出。
    pub(crate) async fn service_with_options(
        &self,
        name: &str,
        options: &RequestOptions,
    ) -> Result<service::ServiceHandle<'_>> {
        let schema = self
            .service_cache
            .lock()
            .await
            .get_or_fetch_detail(self, name, options)
            .await?;
        Ok(service::ServiceHandle::new(self, name.to_string(), schema))
    }

    /// [`method`](Self::method) 的带请求 options 变体：schema 拉取请求
    /// 会以 `options`（叠加在 transport 默认之上）发出；返回的
    /// [`service::MethodHandle`] 的业务调用以 transport 默认 options 为基底。
    pub(crate) async fn method_with_options(
        &self,
        path: &[&str],
        options: &RequestOptions,
    ) -> Result<service::MethodHandle<'_>> {
        if path.len() < 2 {
            tracing::error!(path = ?path, "method path too short");
            return Err(Error::Validation(format!(
                "方法路径至少需要两段 ([\"<service>\", \"<method>\", ...])，但收到: {:?}",
                path
            )));
        }
        let (service_name, method_path) = path.split_first().unwrap();
        self.service_with_options(service_name, options)
            .await?
            .method(method_path)
    }

    /// Resolve a builtin endpoint from the endpoint catalog, attaching the
    /// catalog's `TaskQuery` poll endpoint as a [`wecom_transport::PollEndpoint`]
    /// capability so `TaskQuery` long-task polling resolves the same way.
    pub(crate) fn resolve_builtin_endpoint(&self, key: EndpointKey) -> wecom_transport::Endpoint {
        let poll = self.endpoints.resolve(EndpointKey::TaskQuery);
        self.endpoints
            .resolve(key)
            .with(wecom_transport::PollEndpoint(poll))
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Client");
        s.field("cwd", &self.cwd)
            .field("readable_dirs", &self.readable_dirs)
            .field("writable_dirs", &self.writable_dirs)
            .field("home_dir", &self.home_dir)
            .field("tmp_dir", &self.tmp_dir)
            .field("transport", &self.transport);
        s.finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：Client（主入口）
    //!
    //! ### 关键接口
    //! - [Client::method] — 通过路径切片一步获取 MethodHandle
    //!
    //! ### 关键分支与异常路径
    //! - path 长度 < 2 → Err(Validation)
    //! - 服务不存在（缓存未命中且无网络）→ Err
    //! - 服务存在但方法路径不存在 → Err(Validation)
    //! - 正常路径（顶层方法）→ Ok，name() 正确
    //! - 正常路径（嵌套资源方法）→ Ok，name() 正确
    //!
    //! ### 上下游交互
    //! - 上游：外部调用方直接使用 Client 作为入口
    //! - 下游：委托 [ServiceCache] 加载 schema，再委托 [service::ServiceHandle::method] 解析路径

    use tempfile::TempDir;
    use wecom_transport::EndpointHttpExt;

    use super::*;

    // ── helpers ──

    /// Build a sandboxed Client backed by `root` as both home_dir and tmp_dir.
    fn build_client(root: &std::path::Path) -> Client {
        Client::builder()
            .home_dir(root)
            .tmp_dir(root)
            .readable_dirs(vec![root.to_path_buf()])
            .writable_dirs(vec![root.to_path_buf()])
            .build()
            .unwrap()
    }

    /// Write a minimal service schema JSON into the cache directory so that
    /// `ServiceCache::get_or_fetch_detail` returns it without hitting the network.
    ///
    /// Schema layout:
    /// - top-level method: `list`
    /// - resource `users` with method `get`
    fn seed_service_cache(root: &std::path::Path, service: &str) {
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let schema = serde_json::json!({
            "description": "test service",
            "base_url": "https://test.example.com/",
            "methods": {
                "list": {
                    "http_method": "GET",
                    "path": "/list"
                }
            },
            "resources": {
                "users": {
                    "methods": {
                        "get": {
                            "http_method": "GET",
                            "path": "/users/{id}"
                        }
                    },
                    "resources": {}
                }
            }
        });
        let file = cache_dir.join(format!("service_{}.json", fs::sanitize_filename(service)));
        #[allow(clippy::disallowed_methods)] // Test helper: write fixture outside sandbox
        std::fs::write(file, serde_json::to_string(&schema).unwrap()).unwrap();
    }

    // ── Client::method ──

    /// P1：[Client::method] 路径长度为 0 时返回 Err
    /// 条件：传入空切片 &[]
    /// 断言：返回 Err(Validation)，错误信息包含 "至少需要两段"
    #[tokio::test]
    async fn method_with_empty_path_returns_error() {
        let tmp = TempDir::new().unwrap();
        let client = build_client(tmp.path());
        let err = client.method(&[]).await.unwrap_err();
        assert!(
            err.to_string().contains("至少需要两段"),
            "unexpected error: {err}"
        );
    }

    /// P1：[Client::method] 路径长度为 1 时返回 Err
    /// 条件：传入只含服务名的切片 &["svc"]
    /// 断言：返回 Err(Validation)，错误信息包含 "至少需要两段"
    #[tokio::test]
    async fn method_with_single_segment_returns_error() {
        let tmp = TempDir::new().unwrap();
        let client = build_client(tmp.path());
        let err = client.method(&["svc"]).await.unwrap_err();
        assert!(
            err.to_string().contains("至少需要两段"),
            "unexpected error: {err}"
        );
    }

    /// P0：[Client::method] 正常路径获取顶层方法
    /// 条件：缓存中存在 "svc" 服务，顶层有 "list" 方法，传入 &["svc", "list"]
    /// 断言：返回 Ok，MethodHandle::name() 为 "list"
    #[tokio::test]
    async fn method_returns_top_level_method() {
        let tmp = TempDir::new().unwrap();
        seed_service_cache(tmp.path(), "svc");
        let client = build_client(tmp.path());
        let handle = client.method(&["svc", "list"]).await.unwrap();
        assert_eq!(handle.name(), "list");
    }

    /// P0：[Client::method] 正常路径获取嵌套资源下的方法
    /// 条件：缓存中存在 "svc" 服务，users 资源下有 "get" 方法，传入 &["svc", "users", "get"]
    /// 断言：返回 Ok，MethodHandle::name() 为 "get"
    #[tokio::test]
    async fn method_returns_nested_resource_method() {
        let tmp = TempDir::new().unwrap();
        seed_service_cache(tmp.path(), "svc");
        let client = build_client(tmp.path());
        let handle = client.method(&["svc", "users", "get"]).await.unwrap();
        assert_eq!(handle.name(), "get");
    }

    /// P1：[Client::method] 方法路径不存在时返回 Err
    /// 条件：缓存中存在 "svc" 服务，但不存在 "delete" 方法，传入 &["svc", "delete"]
    /// 断言：返回 Err
    #[tokio::test]
    async fn method_with_nonexistent_method_returns_error() {
        let tmp = TempDir::new().unwrap();
        seed_service_cache(tmp.path(), "svc");
        let client = build_client(tmp.path());
        assert!(client.method(&["svc", "delete"]).await.is_err());
    }

    /// P1：[Client::method] 资源路径不存在时返回 Err
    /// 条件：缓存中存在 "svc" 服务，但不存在 "orders" 资源，传入 &["svc", "orders", "list"]
    /// 断言：返回 Err
    #[tokio::test]
    async fn method_with_nonexistent_resource_returns_error() {
        let tmp = TempDir::new().unwrap();
        seed_service_cache(tmp.path(), "svc");
        let client = build_client(tmp.path());
        assert!(client.method(&["svc", "orders", "list"]).await.is_err());
    }

    // ── Client::endpoint ──

    /// P0：[Client::endpoint] endpoint 携带 path；base_url 为 None（transport 填入）
    /// 条件：以 base_url=https://api.test 构建 client，调用 client.endpoint("/cgi/x")
    /// 断言：endpoint.path == "/cgi/x"，base_url 为 ""
    #[test]
    fn endpoint_carries_path() {
        let tmp = TempDir::new().unwrap();
        let client = Client::builder()
            .home_dir(tmp.path())
            .cwd(tmp.path())
            .build()
            .unwrap();
        let ep = client.endpoint("/cgi/x");
        // base_url is None on the endpoint — transport fills at execute time
        assert_eq!(ep.base_url(), "");
        assert_eq!(ep.path(), "/cgi/x");
    }

    /// P0：[ClientBuilder::transport] 注入带 base_url 的 transport 并构建成功
    /// 条件：通过 builder 注入带 base_url 的 transport
    /// 断言：构建成功
    #[test]
    fn builder_base_url_succeeds() {
        let tmp = TempDir::new().unwrap();
        let transport = wecom_transport::HttpTransportBackend::builder()
            .base_url("https://api.test")
            .build()
            .unwrap();
        let client = Client::builder()
            .home_dir(tmp.path())
            .cwd(tmp.path())
            .transport(transport)
            .build()
            .unwrap();
        // base_url is handled at transport level — just verify client was built
        let _ = client;
    }

    /// P1：[Client::transport_mut] 返回 Transport 可变引用
    /// 条件：构建 client 后调用 transport_mut()
    /// 断言：返回的引用与 transport() 指向同一地址
    #[test]
    fn transport_mut_returns_mutable_reference() {
        let tmp = TempDir::new().unwrap();
        let mut client = build_client(tmp.path());
        let ptr_imm = client.transport() as *const _;
        let ptr_mut = client.transport_mut() as *const _;
        assert_eq!(ptr_imm, ptr_mut);
    }

    /// P1：[Client::resolve_builtin_endpoint] MediaUpload 默认返回 /file/upload 路径，base_url 为 None
    /// 条件：build_client 创建 client
    /// 断言：endpoint.path 包含 "/file/upload"，base_url 为 ""
    #[test]
    fn resolve_builtin_media_upload_path() {
        let tmp = TempDir::new().unwrap();
        let client = build_client(tmp.path());
        let ep = client.resolve_builtin_endpoint(EndpointKey::MediaUpload);
        assert!(ep.path().contains("/file/upload"));
        // base_url is None (transport fills default)
        assert_eq!(ep.base_url(), "");
    }

    /// P2：[Client::Debug] 格式化输出含 Client 字段
    /// 条件：build_client 创建 client
    /// 断言：format!("{:?}", client) 包含 "Client"
    #[test]
    fn debug_fmt_includes_client_fields() {
        let tmp = TempDir::new().unwrap();
        let client = build_client(tmp.path());
        let debug = format!("{client:?}");
        assert!(debug.contains("Client"));
    }
}
