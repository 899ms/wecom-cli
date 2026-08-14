use std::path::PathBuf;
use std::sync::Arc;

use super::Client;
use crate::fs::PathResolver;
use crate::helpers::HelperRegistry;
use crate::registry::ServiceCache;
use crate::{Result, constants};

/// Step-by-step builder for [`Client`].
///
/// Obtain one via [`Client::builder()`](Client::builder), configure it with
/// chainable setters, then call [`build()`](Self::build) to produce the
/// final `Client`.
///
/// Transport 由外部预先构建并通过 [`transport()`](Self::transport) 注入；
/// 未注入时 [`build()`](Self::build) 默认使用 [`wecom_transport::HttpTransportBackend`]。
///
/// # Example
///
/// ```ignore
/// let transport = wecom_transport::HttpTransportBackend::default()
///     .header("Authorization", "Bearer my-token")
///     .build()?;
/// let client = wecom::Client::builder()
///     .home_dir("~/.config/wecom")
///     .transport(transport)
///     .build()?;
/// ```
#[derive(Default)]
pub struct ClientBuilder {
    // -- file-system --
    cwd: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
    readable_dirs: Option<Vec<PathBuf>>,
    writable_dirs: Option<Vec<PathBuf>>,
    path_resolver: Option<PathResolver>,

    // -- cli name --
    /// 外部注入的二进制名（命令名）；未设置时回退 [`constants::DEFAULT_BIN_NAME`]。
    bin_name: Option<String>,

    // -- transport --
    /// 外部注入的 [`wecom_transport::Transport`]。
    /// 设置后 [`build`] 直接采用；未设置时默认使用 [`wecom_transport::HttpTransportBackend`]。
    transport: Option<wecom_transport::Transport>,

    // -- endpoint catalog --
    /// 外部注入的内置 endpoint 配置目录；未设置时使用 [`super::EndpointCatalog::default`]。
    endpoint_catalog: Option<super::EndpointCatalog>,

    // -- custom commands --
    custom_commands: Vec<super::CustomCommand>,

    // -- helpers --
    extra_helpers: Vec<Box<dyn crate::helpers::Helper>>,
}

impl ClientBuilder {
    // -- file-system --

    /// Set the working directory (default: `std::env::current_dir()`).
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Returns the currently configured working directory, if any.
    pub fn get_cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    /// Set the root configuration directory (default `~/.config/wecom`).
    #[must_use]
    pub fn home_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(dir.into());
        self
    }

    /// Returns the currently configured home directory, if any.
    pub fn get_home_dir(&self) -> Option<&PathBuf> {
        self.home_dir.as_ref()
    }

    /// Set the temporary directory (default `$TMPDIR/wecom`).
    #[must_use]
    pub fn tmp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.tmp_dir = Some(dir.into());
        self
    }

    /// Returns the currently configured temporary directory, if any.
    pub fn get_tmp_dir(&self) -> Option<&PathBuf> {
        self.tmp_dir.as_ref()
    }

    /// Set the binary name (command name) shown in `--version`, `--help`
    /// and generated `--doc` usage lines.
    ///
    /// Defaults to [`constants::DEFAULT_BIN_NAME`]; embedders (e.g. the `wecom-cli`
    /// binary) should pass `env!("CARGO_BIN_NAME")` here.
    #[must_use]
    pub fn bin_name(mut self, name: impl Into<String>) -> Self {
        self.bin_name = Some(name.into());
        self
    }

    /// Returns the currently configured binary name, if any.
    pub fn get_bin_name(&self) -> Option<&str> {
        self.bin_name.as_deref()
    }

    /// Add a directory to the sandbox **readable** root list.
    ///
    /// Files under this directory (and its subdirectories) will be allowed for
    /// read operations.
    #[must_use]
    pub fn readable_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.readable_dirs
            .get_or_insert_with(Vec::new)
            .push(dir.into());
        self
    }

    /// Add multiple directories to the sandbox **readable** root list at once.
    #[must_use]
    pub fn readable_dirs(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.readable_dirs.get_or_insert_with(Vec::new).extend(dirs);
        self
    }

    /// Returns the currently configured extra readable directories.
    pub fn get_readable_dirs(&self) -> Option<&[PathBuf]> {
        self.readable_dirs.as_deref()
    }

    /// Add a directory to the sandbox **writable** root list.
    ///
    /// Files under this directory (and its subdirectories) will be allowed for
    /// write, create, and delete operations.
    #[must_use]
    pub fn writable_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.writable_dirs
            .get_or_insert_with(Vec::new)
            .push(dir.into());
        self
    }

    /// Add multiple directories to the sandbox **writable** root list at once.
    #[must_use]
    pub fn writable_dirs(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.writable_dirs.get_or_insert_with(Vec::new).extend(dirs);
        self
    }

    /// Returns the currently configured extra writable directories.
    pub fn get_writable_dirs(&self) -> Option<&[PathBuf]> {
        self.writable_dirs.as_deref()
    }

    /// 注册自定义路径解析器。
    ///
    /// 解析器将调用方传入的路径（相对 / 绝对 / 甚至 `virtual://workspace/...`
    /// 这类非文件系统路径）映射为绝对物理路径。详见 [`PathResolver`]。
    #[must_use]
    pub fn path_resolver(mut self, resolver: PathResolver) -> Self {
        self.path_resolver = Some(resolver);
        self
    }

    /// Inject a fully-constructed [`wecom_transport::Transport`].
    ///
    /// When set, [`build`](Self::build) uses this transport directly.
    /// When not set, [`build`] defaults to [`wecom_transport::HttpTransportBackend`].
    ///
    /// ```ignore
    /// let transport = wecom_transport::HttpTransportBackend::default()
    ///     .header("Authorization", "Bearer my-token")
    ///     .build()?;
    /// let client = wecom::Client::builder().transport(transport).build()?;
    /// ```
    #[must_use]
    pub fn transport(mut self, transport: wecom_transport::Transport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Returns the currently configured transport, if any.
    pub fn get_transport(&self) -> Option<&wecom_transport::Transport> {
        self.transport.as_ref()
    }

    /// 整体替换内置 endpoint 配置目录（详见 [`EndpointCatalog`]）。
    ///
    /// 未调用时 [`build`](Self::build) 使用内建默认表，行为与现状一致；
    /// 调用后可逐 key 覆写媒体上传 / 下载、服务发现、长任务轮询与
    /// schema 方法默认信封的 endpoint。
    #[must_use]
    pub fn endpoint_catalog(mut self, catalog: super::EndpointCatalog) -> Self {
        self.endpoint_catalog = Some(catalog);
        self
    }

    /// 注册一个扩展命令（自定义顶层子命令）。
    ///
    /// 扩展命令注册在服务发现子命令**之前**：与服务同名的服务子命令会被
    /// 跳过（扩展命令优先）。扩展命令与内置命令（`cache` / `schema`）
    /// 同等待遇：跳过服务发现、参与 clap 帮助体系，命中时由
    /// [`CliRun::execute`] 调度到其处理器。详见 [`CustomCommand`](super::CustomCommand)。
    #[must_use]
    pub fn command(mut self, command: super::CustomCommand) -> Self {
        self.custom_commands.push(command);
        self
    }

    /// 注册一个额外的 [`Helper`](crate::helpers::Helper)（产品层 `+` 命令）。
    ///
    /// 内置 helper 之外，调用方可注册自己的产品层 helper；与内置 helper
    /// 同等待遇：按命令路径参与 CLI 命令树构建、帮助体系与调度。
    #[must_use]
    pub fn helper(mut self, helper: impl crate::helpers::Helper + 'static) -> Self {
        self.extra_helpers.push(Box::new(helper));
        self
    }

    // -- build --

    /// Build the [`Client`], resolving paths and creating the HTTP client.
    #[tracing::instrument(level = "debug", name = "client.build", skip_all)]
    pub fn build(self) -> Result<Client> {
        tracing::info!("building client");

        // Emit a compile-time warning in test builds when home_dir is not
        // explicitly set.  This helps catch accidental reads from the real
        // ~/.config/wecom directory.
        #[cfg(test)]
        if self.home_dir.is_none() {
            tracing::info!("ClientBuilder::build() called in test without explicit home_dir");
        }

        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let home_dir = self.home_dir.unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("wecom")
        });

        let tmp_dir = self
            .tmp_dir
            .unwrap_or_else(|| std::env::temp_dir().join("wecom"));

        let mut transport = self
            .transport
            .unwrap_or_else(|| wecom_transport::HttpTransportBackend::default().into());

        if !transport.headers().contains_key("X-WeCom-Cli-Info") {
            transport = transport.with_header(
                "X-WeCom-Cli-Info",
                constants::CLI_INFO.to_json().to_string(),
            )?;
        }

        let mut helper_registry = HelperRegistry::new();
        for helper in self.extra_helpers {
            helper_registry.register(helper);
        }

        let bin_name = self
            .bin_name
            .unwrap_or_else(|| constants::DEFAULT_BIN_NAME.to_string());

        let client = Client {
            // sandboxed file-system defaults
            cwd,
            home_dir,
            tmp_dir,
            readable_dirs: self.readable_dirs.clone(),
            writable_dirs: self.writable_dirs.clone(),

            // runtime
            bin_name,
            transport,

            // endpoint catalog
            endpoints: Arc::new(self.endpoint_catalog.unwrap_or_default()),

            service_cache: ServiceCache::new(),
            helper_registry,
            path_resolver: self.path_resolver,
            custom_commands: self.custom_commands,
        };

        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：ClientBuilder（Client 构建器）
    //!
    //! ### 关键接口
    //! - [ClientBuilder::default] — 创建默认构建器
    //! - [cwd] / [home_dir] / [tmp_dir] — 文件系统路径设置
    //! - [readable_dir] / [writable_dir] — 沙箱目录设置
    //! - [base_url] — API 基础 URL
    //! - [transport] / [get_transport]  — 注入/获取 Transport
    //! - [build] — 构建最终 Client 实例
    //!
    //! ### 关键分支与异常路径
    //! - build 时未注入 transport 则默认使用 HttpTransportBackend
    //! - build 时 `X-WeCom-Cli-Info` header 由 [`HttpTransportBackend::execute`] 自动注入
    //! - build 时未设置 home_dir 则使用 ~/.config/wecom
    //!
    //! ### 上下游交互
    //! - 上游：外部调用方构建 Transport 后注入
    //! - 下游：build 产出 Client 实例

    use wecom_transport::HttpTransportBackend;

    use super::*;

    /// Build an isolated [`ClientBuilder`] (leaked tempdir as `home_dir`/`cwd`).
    fn isolated_builder() -> ClientBuilder {
        let tmp = tempfile::tempdir().expect("failed to create tempdir for test isolation");
        let dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Client::builder().home_dir(&dir).cwd(&dir)
    }

    // ── Default / New ──

    /// P0：默认 ClientBuilder 的所有可选字段均为 None 或空
    /// 条件：通过 [ClientBuilder::default] 创建实例
    /// 断言：cwd、home_dir、tmp_dir、base_url、transport 均为 None
    #[test]
    fn default_builder_has_empty_fields() {
        let b = ClientBuilder::default();
        assert!(b.get_cwd().is_none());
        assert!(b.get_home_dir().is_none());
        assert!(b.get_tmp_dir().is_none());
        assert!(b.get_transport().is_none());
        assert!(b.get_readable_dirs().is_none());
        assert!(b.get_writable_dirs().is_none());
    }

    // ── Custom commands / Helpers ──

    /// P1：[ClientBuilder::command] 注册的扩展命令进入 Client
    /// 条件：注册名为 "auth" 的 [`CustomCommand`] 并 build
    /// 断言：`client.custom_commands()` 恰含该命令，name 匹配
    #[test]
    fn command_registers_custom_command_into_client() {
        let client = isolated_builder()
            .command(crate::client::CustomCommand::new(
                clap::Command::new("auth"),
                |_run, _matches| Box::pin(async { Ok(()) }),
            ))
            .build()
            .unwrap();

        let cmds = client.custom_commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name(), "auth");
    }

    /// P1：[ClientBuilder::command] 可注册多个扩展命令，顺序保持
    /// 条件：依次注册 "auth" / "init" 两个扩展命令并 build
    /// 断言：`client.custom_commands()` 含两者且顺序为注册顺序
    #[test]
    fn command_registers_multiple_custom_commands_in_order() {
        let client = isolated_builder()
            .command(crate::client::CustomCommand::new(
                clap::Command::new("auth"),
                |_run, _matches| Box::pin(async { Ok(()) }),
            ))
            .command(crate::client::CustomCommand::new(
                clap::Command::new("init"),
                |_run, _matches| Box::pin(async { Ok(()) }),
            ))
            .build()
            .unwrap();

        let cmds = client.custom_commands();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name(), "auth");
        assert_eq!(cmds[1].name(), "init");
    }

    struct NoopHelper;

    impl crate::helpers::Helper for NoopHelper {
        fn path(&self) -> Vec<&'static str> {
            vec!["svc"]
        }
        fn about(&self) -> crate::helpers::HelperMeta {
            crate::helpers::HelperMeta::new("+noop", "A no-op helper")
        }
        fn execute<'a>(
            &'a self,
            _run: &'a crate::client::CliRun<'a>,
            _params: serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    /// P1：[ClientBuilder::helper] 注册的 helper 进入 Client 的 HelperRegistry
    /// 条件：注册 path=["svc"]、name="+noop" 的 helper 并 build
    /// 断言：`helper_registry().get_helper(&["svc", "+noop"])` 命中；
    ///       注册的 helper 成为顶层分组（`get_helpers_in(&[])` 的 children 含 "svc"）
    ///
    /// 注：本项目默认 `HelperRegistry` 为空（不内置 media helpers），故此处断言注册的
    /// helper 自身成为顶层分组。
    #[test]
    fn helper_registers_into_helper_registry() {
        let client = isolated_builder().helper(NoopHelper).build().unwrap();

        assert!(
            client
                .helper_registry()
                .get_helper(&["svc", "+noop"])
                .is_some(),
            "registered helper not found in registry"
        );
        let (_, children) = client.helper_registry().get_helpers_in(&[]);
        assert!(
            children.contains("svc"),
            "registered helper should appear as a top-level group"
        );
    }

    /// P1：[ClientBuilder::helper] 可注册多个 helper，全部进入 HelperRegistry
    /// 条件：注册 path 分别为 ["svc"] 与 ["svc","sub"] 的两个 helper 并 build
    /// 断言：`helper_registry()` 分别按各自路径命中
    #[test]
    fn helper_registers_multiple_helpers() {
        struct SubHelper;
        impl crate::helpers::Helper for SubHelper {
            fn path(&self) -> Vec<&'static str> {
                vec!["svc", "sub"]
            }
            fn about(&self) -> crate::helpers::HelperMeta {
                crate::helpers::HelperMeta::new("+sub", "A sub-level no-op helper")
            }
            fn execute<'a>(
                &'a self,
                _run: &'a crate::client::CliRun<'a>,
                _params: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
            {
                Box::pin(async { Ok(()) })
            }
        }

        let client = isolated_builder()
            .helper(NoopHelper)
            .helper(SubHelper)
            .build()
            .unwrap();

        let reg = client.helper_registry();
        assert!(
            reg.get_helper(&["svc", "+noop"]).is_some(),
            "first helper not registered"
        );
        assert!(
            reg.get_helper(&["svc", "sub", "+sub"]).is_some(),
            "second helper not registered"
        );
    }

    // ── cwd setter/getter ──

    /// P0：cwd setter/getter 基本功能
    /// 条件：调用 [cwd]
    /// 断言：[get_cwd] 返回 [Some]
    #[test]
    fn cwd_setter_and_getter() {
        let b = ClientBuilder::default().cwd("/project");
        assert_eq!(b.get_cwd().unwrap(), &std::path::PathBuf::from("/project"));
    }

    /// P1：cwd 多次设置时后者覆盖前者
    /// 条件：链式调用 [cwd] 两次传入不同值
    /// 断言：最终值为第二次设置的值
    #[test]
    fn cwd_can_overwrite() {
        let b = ClientBuilder::default().cwd("/first").cwd("/second");
        assert_eq!(b.get_cwd().unwrap(), &std::path::PathBuf::from("/second"));
    }

    // ── home_dir setter/getter ──

    /// P0：home_dir setter/getter 基本功能
    /// 条件：调用 [home_dir]
    /// 断言：[get_home_dir] 返回 [Some]
    #[test]
    fn home_dir_setter_and_getter() {
        let b = ClientBuilder::default().home_dir("/home/user/.config/wecom");
        assert_eq!(
            b.get_home_dir().unwrap(),
            &std::path::PathBuf::from("/home/user/.config/wecom")
        );
    }

    // ── tmp_dir setter/getter ──

    /// P0：tmp_dir setter/getter 基本功能
    /// 条件：调用 [tmp_dir]
    /// 断言：[get_tmp_dir] 返回 [Some]
    #[test]
    fn tmp_dir_setter_and_getter() {
        let b = ClientBuilder::default().tmp_dir("/custom/tmp");
        assert_eq!(
            b.get_tmp_dir().unwrap(),
            &std::path::PathBuf::from("/custom/tmp")
        );
    }

    // ── readable_dirs ──

    /// P0：readable_dir 添加单个可读目录
    /// 条件：调用 [readable_dir]
    /// 断言：[get_readable_dirs] 返回 [Some]
    #[test]
    fn readable_dir_adds_single_dir() {
        let b = ClientBuilder::default().readable_dir("/data");
        let dirs = b.get_readable_dirs().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(&dirs[0], &std::path::PathBuf::from("/data"));
    }

    /// P1：readable_dirs 批量添加多个可读目录
    /// 条件：调用 [readable_dirs]
    /// 断言：[get_readable_dirs] 返回包含两个路径的切片
    #[test]
    fn readable_dirs_adds_multiple_dirs() {
        let b = ClientBuilder::default().readable_dirs(vec![
            std::path::PathBuf::from("/data1"),
            std::path::PathBuf::from("/data2"),
        ]);
        let dirs = b.get_readable_dirs().unwrap();
        assert_eq!(dirs.len(), 2);
    }

    /// P1：readable_dir 可链式调用添加多个目录
    /// 条件：连续调用两次 [readable_dir]
    /// 断言：总目录数为 2
    #[test]
    fn readable_dir_chain_multiple() {
        let b = ClientBuilder::default()
            .readable_dir("/data1")
            .readable_dir("/data2");
        assert_eq!(b.get_readable_dirs().unwrap().len(), 2);
    }

    // ── writable_dirs ──

    /// P0：writable_dir 添加单个可写目录
    /// 条件：调用 [writable_dir]
    /// 断言：[get_writable_dirs] 返回 [Some]
    #[test]
    fn writable_dir_adds_single_dir() {
        let b = ClientBuilder::default().writable_dir("/output");
        let dirs = b.get_writable_dirs().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], std::path::PathBuf::from("/output"));
    }

    /// P1：writable_dirs 批量添加多个可写目录
    /// 条件：调用 [writable_dirs]
    /// 断言：[get_writable_dirs] 返回包含两个路径的切片
    #[test]
    fn writable_dirs_adds_multiple_dirs() {
        let b = ClientBuilder::default().writable_dirs(vec![
            std::path::PathBuf::from("/out1"),
            std::path::PathBuf::from("/out2"),
        ]);
        let dirs = b.get_writable_dirs().unwrap();
        assert_eq!(dirs.len(), 2);
    }

    // ── transport ──

    /// P0：transport setter/getter 基本功能
    /// 条件：调用 [transport] 注入 HttpTransportBackend
    /// 断言：[get_transport] 返回 Some，name() 为 "http"
    #[test]
    fn transport_setter_and_getter() {
        let transport: wecom_transport::Transport = HttpTransportBackend::default().into();
        let b = ClientBuilder::default().transport(transport);
        assert_eq!(b.get_transport().unwrap().name(), "http");
    }

    /// P0：[ClientBuilder::build] 注入的 Transport 在 build 后生效
    /// 条件：注入带自定义 base_url 的 HttpTransportBackend
    /// 断言：build() 后 client.transport().name() == "http"
    #[test]
    fn build_uses_injected_transport() -> Result<()> {
        let transport: wecom_transport::Transport = HttpTransportBackend::builder()
            .base_url("http://injected")
            .build()?;
        let client = ClientBuilder::default().transport(transport).build()?;
        assert_eq!(client.transport().name(), "http");
        Ok(())
    }

    /// P0：[ClientBuilder::build] 未注入 Transport 时默认使用 HttpTransportBackend
    /// 条件：不调用 transport()
    /// 断言：client.transport().name() == "http"
    #[test]
    fn build_defaults_to_http_transport() -> Result<()> {
        let client = ClientBuilder::default().build()?;
        assert_eq!(client.transport().name(), "http");
        Ok(())
    }

    // ── build() ──

    /// P0：build() 成功构建 Client（使用默认 HttpTransportBackend）
    /// 条件：使用默认 ClientBuilder 调用 build()
    /// 断言：返回 Ok(Client)
    #[test]
    fn build_succeeds_with_defaults() -> Result<()> {
        let _client = ClientBuilder::default().build()?;
        Ok(())
    }

    /// P1：build() 设置的 cwd 正确传递到 Client
    /// 条件：设置 cwd 后 build()
    /// 断言：Client 的 cwd() 等于设置的值
    #[test]
    fn build_passes_cwd_to_client() -> Result<()> {
        let client = ClientBuilder::default().cwd("/tmp").build()?;
        assert_eq!(client.cwd(), std::path::Path::new("/tmp"));
        Ok(())
    }

    /// P1：build() 设置的 tmp_dir 正确传递到 Client
    /// 条件：设置 tmp_dir 后 build()
    /// 断言：Client 的 tmp_dir() 等于设置的值
    #[test]
    fn build_passes_tmp_dir_to_client() -> Result<()> {
        let client = ClientBuilder::default().tmp_dir("/custom/tmp").build()?;
        assert_eq!(client.tmp_dir(), std::path::Path::new("/custom/tmp"));
        Ok(())
    }

    /// P1：[ClientBuilder::build] 不需要任何额外配置就可以构建
    /// 条件：仅设置 home_dir 和 cwd，不设置 base_url 等可选字段
    /// 断言：build() 返回 Ok
    #[test]
    fn build_without_any_config_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let result = ClientBuilder::default()
            .home_dir(tmp.path())
            .cwd(tmp.path())
            .build();
        assert!(result.is_ok());
    }
}
