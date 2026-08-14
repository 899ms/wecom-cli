use std::future::IntoFuture;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anstyle::{AnsiColor, Color, Style};
use clap::{ArgMatches, Command};
use indexmap::IndexMap;
use wecom_transport::Error as TransportError;

use super::Client;
use crate::telemetry::contract::subcmd_not_found as ctr_snf;
use crate::{ERRCODE_SHOW_HELP, Error, Result, commands, constants, fs, service, telemetry};

/// `error:` 前缀样式，对齐 clap 默认错误风格（红色加粗）。
const ERROR_PREFIX_STYLE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::Red)))
    .bold();

// ── CliRunOutput ────────────────────────────────────────────────

/// Thread-safe writer shared across the `run` pipeline for all output.
///
/// Defaults to `stdout`. Library embedders can supply any `Write`
/// implementation (e.g. `Vec<u8>`, a file, a network socket).
pub type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// 服务端额外数据回调类型（与 PollCallback 模式一致，使用 Arc 共享所有权）。
///
/// 当后台 API 响应中除了主 `result` 还附带 side-channel 字段时，
/// 通过此回调将额外数据透传给调用方。transport 层不做解析，纯透传。
pub(crate) type ExtraDataCallback = Arc<dyn Fn(&IndexMap<String, serde_json::Value>) + Send + Sync>;

/// Output configuration for the `run` pipeline.
///
/// Encapsulates *where* and *how* CLI output is written. This is a
/// parameter of the `run` family of methods — not a property of
/// [`Client`](super::Client).
///
/// # Default
///
/// [`CliRunOutput::default()`] writes to **stdout** with ANSI color
/// auto-detected based on whether stdout is a terminal.
///
/// # Examples
///
/// ```ignore
/// // Capture output in a buffer:
/// let buf: Vec<u8> = Vec::new();
/// let output = CliRunOutput::new(buf);
///
/// // Force color on:
/// let output = CliRunOutput::stdout().force_color(true);
/// ```
pub struct CliRunOutput {
    writer: Writer,
    force_color: bool,
}

impl Default for CliRunOutput {
    /// Create a `CliRunOutput` that writes to **stdout** with color
    /// auto-detected.
    fn default() -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(std::io::stdout()))),
            force_color: std::io::stdout().is_terminal(),
        }
    }
}

impl CliRunOutput {
    /// Create a `CliRunOutput` backed by a custom writer.
    ///
    /// Color is **disabled** by default when using a custom writer.
    /// Call [`.force_color(true)`](Self::force_color) to override.
    pub fn new(w: impl Write + Send + 'static) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(w))),
            force_color: false,
        }
    }

    /// Create a `CliRunOutput` that writes to **stdout** with color
    /// auto-detected.
    ///
    /// Equivalent to [`CliRunOutput::default()`].
    pub fn stdout() -> Self {
        Self::default()
    }

    /// Create a `CliRunOutput` backed by a pre-built [`Writer`] arc.
    ///
    /// Useful when the caller needs to retain a handle to the same
    /// `Arc` in order to inspect what was written (e.g. in tests).
    pub fn from_writer_arc(w: Writer) -> Self {
        Self {
            writer: w,
            force_color: false,
        }
    }

    /// Create a new `CliRunOutput` that shares the same underlying writer
    /// and color setting.
    ///
    /// This is cheap (only clones an `Arc`) and is useful when multiple
    /// consumers need to write to the same output destination.
    pub fn clone_shared(&self) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
            force_color: self.force_color,
        }
    }

    /// Force ANSI color output on or off (builder pattern).
    #[must_use]
    pub fn force_color(mut self, enable: bool) -> Self {
        self.force_color = enable;
        self
    }

    // ── Accessors ──

    /// Returns a reference to the shared [`Writer`].
    ///
    /// Callers can lock it to perform multiple writes atomically:
    /// ```ignore
    /// let mut w = output.writer().lock().unwrap();
    /// writeln!(w, "hello")?;
    /// ```
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Whether forced ANSI color output is enabled.
    pub fn is_force_color(&self) -> bool {
        self.force_color
    }

    /// Write a line of text to the configured writer.
    ///
    /// Convenience wrapper that appends a newline and silently ignores
    /// write errors.
    pub fn print(&self, s: &str) {
        let _ = writeln!(self.writer.lock().unwrap_or_else(|e| e.into_inner()), "{s}");
    }

    /// Render a [`StyledStr`](clap::builder::StyledStr) to a color-aware `String`.
    ///
    /// When `force_color` is enabled the ANSI escape codes embedded in the
    /// `StyledStr` are preserved; otherwise they are stripped via the
    /// `Display` implementation. This is the single source of truth for
    /// converting styled clap output into a printable string.
    pub fn render_styled(&self, s: &clap::builder::StyledStr) -> String {
        if self.force_color {
            s.ansi().to_string()
        } else {
            s.to_string()
        }
    }

    /// Write a [`StyledStr`](clap::builder::StyledStr) to the configured writer.
    ///
    /// Color-aware: see [`render_styled`](Self::render_styled).
    pub fn print_styled(&self, s: &clap::builder::StyledStr) {
        self.print(&self.render_styled(s));
    }
}

impl std::fmt::Debug for CliRunOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliRunOutput")
            .field("force_color", &self.force_color)
            .finish_non_exhaustive()
    }
}

// ── CliRun ───────────────────────────────────────────────────

/// A CLI run context returned by [`Client::run()`] that can be awaited
/// directly or configured with extra options before execution.
///
/// `CliRun` bundles the [`Client`] reference, output configuration, and
/// additional HTTP headers into a single value that is threaded through
/// the entire CLI command pipeline.
///
/// You can chain `.output()` / `.headers()` / `.header()` to configure
/// the run before `.await`ing.
///
/// # Examples
///
/// ```ignore
/// // Simple – no extra options:
/// client.run(argv).await?;
///
/// // With custom output:
/// client.run(argv).output(CliRunOutput::new(buf)).await?;
///
/// // With additional headers:
/// client.run(argv).headers(&my_headers).await?;
///
/// // With a single header:
/// client.run(argv).header("x-custom", "value").await?;
///
/// // Override working directory for this run:
/// client.run(argv).cwd("/tmp/workspace").await?;
///
/// // Restrict sandbox readable/writable directories:
/// client.run(argv)
///     .readable_dirs(vec![PathBuf::from("/data/input")])
///     .writable_dirs(vec![PathBuf::from("/data/output")])
///     .await?;
///
/// // Combine fs overrides with other options:
/// client.run(argv)
///     .cwd("/project")
///     .writable_dirs(vec![PathBuf::from("/project/out")])
///     .home_dir("/custom/home")
///     .headers(&my_headers)
///     .await?;
///
/// // Cap every individual wire call (首发请求、分页每页、长任务轮询每一轮
/// // /task/query、媒体上传子请求) with a uniform per-request
/// // timeout. 注意：这是"每笔请求"的超时，而不是整个 run 的总挂钟时间。
/// client.run(argv)
///     .timeout(std::time::Duration::from_secs(30))
///     .await?;
///
/// // Receive a heartbeat per long-task polling round (None when the server
/// // hasn't published progress in this round; useful as a "still alive" signal).
/// // `ev.result` is `Option<&serde_json::Value>` — already parsed.
/// client.run(argv)
///     .on_poll(|ev| eprintln!("[heartbeat] task={} result={:?}", ev.taskid, ev.result))
///     .await?;
/// ```
pub struct CliRun<'a> {
    client: &'a Client,
    fs: fs::Fs,
    argv: Vec<String>,
    output: CliRunOutput,
    header_error: Option<Error>,
    home_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
    options: wecom_transport::RequestOptions,
    on_extra_data: Option<ExtraDataCallback>,
}

impl<'a> std::fmt::Debug for CliRun<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliRun")
            .field("argv", &self.argv)
            .field("output", &self.output)
            .field("options", &self.options)
            .field(
                "on_extra_data",
                &self.on_extra_data.as_ref().map(|_| "<callback>"),
            )
            .finish()
    }
}

// Implements inherent `.headers()` / `.header()` / `.timeout()` /
// `.on_poll()` / `.with_options()` methods so callers
// never need to import a trait.
//
// `+options` 的语义：替换 `+timeout` 和 `+on_poll`，生成：
// - `with_options(RequestOptions)` — 整体替换所有 per-request 参数
// - `timeout(Duration)` — 设置每笔独立请求的超时
// - `on_poll(F)` / `on_poll_arc(cb)` — 长任务轮询心跳回调
// - `extension(T)` / `extensions(&Extensions)` / `get_extensions()` —
//   扩展袋注入 / 合并 / 读取
wecom_transport::impl_request_builder!(
    CliRun<'a>,
    +options,
    error_type = Error,
    error_wrapper = Error::Other,
);

impl<'a> CliRun<'a> {
    // ── Client ──

    /// Returns a reference to the [`Client`].
    pub fn get_client(&self) -> &Client {
        self.client
    }

    // ── fs ──

    /// Return the [`fs::Fs`] handle for this run.
    ///
    /// The `Fs` is built at construction time from the client defaults
    /// and can be overridden via `.cwd()`, `.readable_dirs()`,
    /// `.writable_dirs()`, or `.fs_config()`.
    pub fn fs(&self) -> &fs::Fs {
        &self.fs
    }

    /// Return a mutable reference to the [`fs::Fs`] handle.
    pub fn fs_mut(&mut self) -> &mut fs::Fs {
        &mut self.fs
    }

    /// Override the working directory (used for resolving relative paths)
    /// for this run only.  Rebuilds the internal `Fs`.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.fs.set_cwd(dir);
        self
    }

    /// Override the sandbox readable root directories for this run only.
    #[must_use]
    pub fn readable_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        *self.fs.readable_dirs_mut() = Some(dirs);
        self
    }

    /// Override the sandbox writable root directories for this run only.
    #[must_use]
    pub fn writable_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        *self.fs.writable_dirs_mut() = Some(dirs);
        self
    }

    /// Override the custom path resolver for this run only.
    #[must_use]
    pub fn resolver(mut self, resolver: crate::fs::PathResolver) -> Self {
        *self.fs.resolver_mut() = Some(resolver);
        self
    }

    // ── home_dir ──

    /// Override the home directory for this run only.
    ///
    /// If not set, falls back to [`Client::home_dir()`].
    #[must_use]
    pub fn home_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(dir.into());
        self
    }

    /// Effective home directory: per-run override or client default.
    pub fn get_home_dir(&self) -> &Path {
        self.home_dir
            .as_deref()
            .unwrap_or_else(|| self.client.home_dir())
    }

    /// Effective cache directory (derived from [`get_home_dir`](Self::get_home_dir)).
    pub fn get_cache_dir(&self) -> PathBuf {
        self.get_home_dir().join("cache")
    }

    // ── tmp_dir ──

    /// Override the temporary directory for this run only.
    ///
    /// If not set, falls back to [`Client::tmp_dir()`].
    #[must_use]
    pub fn tmp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.tmp_dir = Some(dir.into());
        self
    }

    /// Effective temporary directory: per-run override or client default.
    pub fn get_tmp_dir(&self) -> &Path {
        self.tmp_dir
            .as_deref()
            .unwrap_or_else(|| self.client.tmp_dir())
    }

    /// Effective request storage directory (derived from [`get_tmp_dir`](Self::get_tmp_dir)).
    pub fn get_request_storage_dir(&self) -> PathBuf {
        self.get_tmp_dir().join("requests")
    }

    // ── output ──

    /// Replace the output configuration.
    #[must_use]
    pub fn output(mut self, output: CliRunOutput) -> Self {
        self.output = output;
        self
    }

    /// Returns a reference to the [`CliRunOutput`] configuration.
    pub fn get_output(&self) -> &CliRunOutput {
        &self.output
    }

    /// 注册一个回调，在每次非二进制响应携带服务端额外数据（非空）时接收
    /// side-channel 字段。
    ///
    /// 单页模式下服务端有额外数据则触发 1 次；分页模式下每页各自触发。
    /// 额外数据为空时回调不触发。多次调用以最后一次为准（last-one-wins）。
    ///
    /// 回调签名 `&IndexMap<String, Value>` 与
    /// [`wecom_transport::ExecuteOutput::extra`] /
    /// [`wecom_transport::PollEvent::extra`] 类型一致。
    ///
    /// # Examples
    ///
    /// ```ignore
    /// client.run(argv)
    ///     .on_extra_data(|data| {
    ///         if let Some(display) = data.get("display_result") {
    ///             eprintln!("展示结果: {display}");
    ///         }
    ///     })
    ///     .await?;
    /// ```
    #[must_use]
    pub fn on_extra_data<F>(mut self, f: F) -> Self
    where
        F: Fn(&IndexMap<String, serde_json::Value>) + Send + Sync + 'static,
    {
        self.on_extra_data = Some(Arc::new(f));
        self
    }

    /// 内部使用：返回已注册的额外数据回调（若有）。
    pub(crate) fn get_on_extra_data(&self) -> Option<&ExtraDataCallback> {
        self.on_extra_data.as_ref()
    }

    /// Render clap help / error output color-aware (see
    /// [`CliRunOutput::render_styled`]), preserving the original styling.
    ///
    /// Shared between `--help` rendering ([`service::handler`]) and the CLI
    /// parse-error path ([`execute`](Self::execute)).
    pub(crate) fn render_help_message(&self, fallback: &clap::builder::StyledStr) -> String {
        self.output.render_styled(fallback)
    }

    // ── Execute ──

    /// Execute the CLI command.
    ///
    /// This is called automatically when you `.await` the [`CliRun`],
    /// but can also be invoked explicitly if needed.
    ///
    /// ## Tracing
    /// Opens a `cli.run` span. All downstream spans (`service.execute`,
    /// `transport.invoke`, `http.request`) nest under it
    /// automatically. The resolved subcommand path is recorded as the
    /// span's [`contract::subcmd::FIELD_PATH`] field and delivered at
    /// span close through
    /// [`SubcmdExt::on_subcmd`](crate::telemetry::SubcmdExt::on_subcmd).
    #[tracing::instrument(
        level = "info",
        name = "cli.run",
        skip_all,
        fields(subcmd = tracing::field::Empty),
    )]
    pub async fn execute(mut self) -> Result<()> {
        if let Some(e) = self.header_error {
            return Err(e);
        }
        let first_arg = self
            .argv
            .iter()
            .skip(1)
            .find(|a| *a == "-V" || *a == "--version" || !a.starts_with('-'))
            .cloned()
            .unwrap_or_default();

        tracing::info!(%first_arg, "execute start");

        if first_arg == "-V" || first_arg == "--version" {
            self.output
                .print(&constants::CLI_INFO.display_with_name(self.client.bin_name()));
            return Ok(());
        }

        let mut cmd = Command::new(self.client.bin_name().to_owned())
            .version(constants::CLI_INFO.display_with_name(self.client.bin_name()))
            .arg_required_else_help(true);

        let post_subcmds = vec![
            commands::cache::build_cache_cmd(),
            commands::schema::build_schema_cmd(),
        ];
        let custom_cmds = || self.client.custom_commands().iter();
        let is_post_subcmd = post_subcmds.iter().any(|c| c.get_name() == first_arg)
            || custom_cmds().any(|c| c.name() == first_arg);

        // 扩展命令注册在服务发现子命令之前。
        cmd = cmd.subcommands(custom_cmds().map(|c| c.command().clone()));

        if !is_post_subcmd && cmd.find_subcommand(&first_arg).is_none() {
            for info in self
                .client
                .list_services_with_options(self.get_options())
                .await?
                .iter()
            {
                // 扩展命令优先：跳过与扩展命令同名的服务。
                if custom_cmds().any(|c| c.name() == info.name) {
                    tracing::warn!(
                        service = %info.name,
                        "service shadowed by custom command, skipped"
                    );
                    continue;
                }
                let schema = if first_arg == info.name {
                    let service = self
                        .client
                        .service_with_options(&info.name, self.get_options())
                        .await?;
                    Some(service.schema)
                } else {
                    None
                };
                cmd = cmd.subcommand(service::build_service_cmd(
                    &self.client.helper_registry,
                    info,
                    schema.as_deref(),
                ));
            }
        }

        cmd = cmd.subcommands(post_subcmds);

        let argv = std::mem::take(&mut self.argv);
        let matches = match cmd.try_get_matches_from_mut(argv.clone()) {
            Ok(m) => m,
            Err(e) => {
                let is_err = e.use_stderr();

                // Real clap error: its own rendered output is the fallback
                // (already styled by clap; preserved color-aware).
                let rendered = e.render();
                let message = self.output.render_styled(&rendered);

                if !is_err {
                    self.output.print(&message);
                    return Ok(());
                }

                if e.kind() == clap::error::ErrorKind::InvalidSubcommand {
                    // Collect the full subcommand path from argv: all
                    // consecutive non-flag args after argv[0], joined by " ",
                    // stopping at the first arg that starts with '-'.
                    let full_subcmd = argv
                        .iter()
                        .skip(1)
                        .take_while(|a| !a.starts_with('-'))
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    telemetry::emit(
                        ctr_snf::KIND,
                        &serde_json::json!({ ctr_snf::FIELD_SUBCMD: full_subcmd }),
                    );
                }

                return Err(Error::CliOutput {
                    code: 2,
                    message,
                    source: Some(e),
                });
            }
        };

        let subcmd_path = extract_subcmd_path(&matches);
        tracing::Span::current().record("subcmd", &subcmd_path);
        tracing::info!(subcmd = %subcmd_path, "dispatching to subcommand");

        let result = match matches.subcommand() {
            Some(("cache", matches)) => commands::cache::handle_cache_cmd(&self, matches).await,
            Some(("schema", matches)) => commands::schema::handle_schema_cmd(&self, matches).await,
            Some((name, matches)) => {
                if let Some(custom) = custom_cmds().find(|c| c.name() == name) {
                    custom.handle(&self, matches).await
                } else {
                    service::handle_service_cmd(&self, name, matches, &cmd).await
                }
            }
            None => Err(Error::Other("Missing subcommand".into())),
        };

        // 后台接口返回 10021 错误码时，视为参数/用法错误：渲染「error 行 + 当前
        // 命令 help」（对齐 clap 用法错误输出格式）后，以 `CliOutput` 返回——退出码
        // 2、render 直接输出已渲染文本，与正常 help/用法错误走同一套处理路径。
        if let Err(Error::Transport(
            api @ TransportError::Api {
                code: Some(ERRCODE_SHOW_HELP),
                ..
            },
        )) = &result
        {
            let leaf_path = extract_subcmd_path(&matches);
            let path: Vec<&str> = leaf_path.split(' ').collect();
            return Err(Error::CliOutput {
                code: 2,
                message: self.render_leaf_help(&cmd, &path, Some(api)),
                source: None,
            });
        }

        result
    }

    /// Walk the command tree along `path` (service name first, then method /
    /// helper segments) to the leaf subcommand and render its clap help text.
    /// When `api_error` is `Some`, the output is prefixed with an `error:` line
    /// (aligned with clap's usage-error output format); otherwise the raw help
    /// is used.
    pub(crate) fn render_leaf_help(
        &self,
        cmd: &Command,
        path: &[&str],
        api_error: Option<&TransportError>,
    ) -> String {
        let mut sub = cmd;
        for seg in path {
            match sub.find_subcommand(seg) {
                Some(child) => sub = child,
                None => break,
            }
        }
        let help = sub.clone().render_help();
        let fallback = match api_error {
            Some(e) => clap::builder::StyledStr::from(format!(
                "{}error:{} {}\n\n{}",
                ERROR_PREFIX_STYLE.render(),
                ERROR_PREFIX_STYLE.render_reset(),
                e.message(),
                help.ansi()
            )),
            None => help,
        };
        self.render_help_message(&fallback)
    }
}

impl<'a> IntoFuture for CliRun<'a> {
    type Output = Result<()>;
    type IntoFuture = Pin<Box<dyn std::future::Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.execute())
    }
}

impl Client {
    /// 核心入口：接受命令行参数列表。
    ///
    /// `argv` 应包含程序名本身（即 `std::env::args().collect()` 的完整结果）。
    ///
    /// Returns a [`CliRun`] that can be `.await`ed directly, or
    /// configured with `.output()` / `.headers()` / `.header()` first.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Simple usage (output to stdout):
    /// client.run(argv).await?;
    ///
    /// // With custom output:
    /// client.run(argv).output(CliRunOutput::new(buf)).await?;
    ///
    /// // With additional headers:
    /// client.run(argv).headers(&headers).await?;
    /// ```
    pub fn run(&self, argv: Vec<String>) -> CliRun<'_> {
        CliRun {
            client: self,
            fs: self.default_fs(),
            argv,
            output: CliRunOutput::default(),
            header_error: None,
            home_dir: None,
            tmp_dir: None,
            options: wecom_transport::RequestOptions::default(),
            on_extra_data: None,
        }
    }
}

/// Extract the full subcommand path from clap-parsed [`ArgMatches`].
///
/// Walks the subcommand chain recursively and returns a space-separated
/// path string (e.g. `"contact users search"`), matching the format used
/// by [`method_alias`](crate::telemetry::contract::method_alias) events.
fn extract_subcmd_path(matches: &ArgMatches) -> String {
    let mut path: Vec<&str> = Vec::new();
    let mut cur = matches;
    while let Some((name, sub)) = cur.subcommand() {
        path.push(name);
        cur = sub;
    }
    path.join(" ")
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：run（CLI 入口、CliRun 与 RunOutput）
    //!
    //! ### 关键接口
    //! - [Client::run] — 创建 [CliRun]，支持 `.await` 或 `.headers()` / `.header()` / `.timeout()` / `.on_poll()` 链式调用
    //! - `CliRun` — inherent `.headers()` / `.header()` / `.timeout()` / `.on_poll()` methods (via `impl_request_builder!` macro)
    //! - [CliRun::execute] — 执行 CLI 命令（可由 [IntoFuture] 自动调用）
    //! - `IntoFuture for CliRun` — 支持 `.await` 语法
    //! - [CliRunOutput::default] — 默认 stdout + 自动检测颜色
    //! - [CliRunOutput::new] — 自定义 writer，颜色默认关闭
    //! - [CliRunOutput::stdout] — 等同于 default
    //! - [CliRunOutput::from_writer_arc] — 从预构建 Writer arc 创建
    //! - [CliRunOutput::force_color] — 强制开关颜色（builder pattern）
    //! - [CliRunOutput::print] — 写入一行文本
    //!
    //! ### 关键分支与异常路径
    //! - `argv` 含 `-V` / `--version` → 输出版本号并提前返回
    //! - 子命令未找到 → 动态注册服务子命令后重试匹配
    //! - 子命令匹配失败 → 返回 [Error::CliOutput]（携带预渲染 message 与原始 `clap::Error` 作为 source；非错误路径）或 [Error::Other]
    //! - `headers` 非空 → 通过 [CliRun] 传递给下游
    //! - 自定义 writer 默认 force_color=false
    //! - stdout 默认 force_color=is_terminal()
    //! - print 在 writer poisoned 时不 panic
    //!
    //! ### 上下游交互
    //! - 上游：binary `wecom` 的 `main()` 调用 `client.run(argv).await`
    //! - 下游：委托 [commands::cache]、[commands::schema]、[service::handle_service_cmd] 处理具体子命令

    use super::*;

    /// Build an isolated [Client] for unit tests.
    ///
    /// Uses a leaked tempdir as `home_dir` so that tests never touch
    /// the real `~/.config/wecom` directory.
    fn build_isolated_client() -> Client {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Client::builder().home_dir(&dir).cwd(&dir).build().unwrap()
    }

    // ── CliRunOutput ──

    /// A cloneable, `Write`-compatible buffer for capturing output in tests.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// P0：[CliRunOutput::new] 自定义 writer 默认 force_color 为 false
    /// 条件：使用 Vec<u8> 创建 CliRunOutput
    /// 断言：is_force_color() 返回 false
    #[test]
    fn new_custom_writer_disables_color() {
        let output = CliRunOutput::new(Vec::<u8>::new());
        assert!(!output.is_force_color());
    }

    /// P0：[CliRunOutput::force_color] builder 方法正确设置颜色
    /// 条件：创建后调用 .force_color(true)
    /// 断言：is_force_color() 返回 true
    #[test]
    fn force_color_builder_sets_flag() {
        let output = CliRunOutput::new(Vec::<u8>::new()).force_color(true);
        assert!(output.is_force_color());
    }

    /// P0：[CliRunOutput::print] 写入内容到自定义 writer
    /// 条件：使用 SharedBuf 创建 RunOutput，调用 print("hello")
    /// 断言：writer 中包含 "hello\n"
    #[test]
    fn print_writes_to_custom_writer() {
        let buf = SharedBuf::new();
        let output = CliRunOutput::new(buf.clone());
        output.print("hello");
        assert_eq!(buf.contents(), "hello\n");
    }

    /// P1：[CliRunOutput::from_writer_arc] 从预构建 arc 创建
    /// 条件：传入预构建的 Writer arc
    /// 断言：writer() 返回相同的 arc
    #[test]
    fn from_writer_arc_shares_reference() {
        let buf: Writer = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let output = CliRunOutput::from_writer_arc(buf.clone());
        assert!(Arc::ptr_eq(output.writer(), &buf));
    }

    /// P1：[CliRunOutput] 实现 Debug
    /// 条件：创建 CliRunOutput
    /// 断言：Debug 输出包含 "CliRunOutput" 和 "force_color"
    #[test]
    fn debug_impl() {
        let output = CliRunOutput::new(std::io::sink());
        let debug = format!("{output:?}");
        assert!(debug.contains("CliRunOutput"));
        assert!(debug.contains("force_color"));
    }

    /// P1：[CliRunOutput::render_styled] force_color 关闭时剥离 ANSI 样式
    /// 条件：构造带色 StyledStr，output 未开启 force_color
    /// 断言：返回串不含 ESC(\x1b) 转义，仅保留纯文本
    #[test]
    fn render_styled_strips_ansi_when_color_off() {
        let mut styled = clap::builder::StyledStr::new();
        use std::fmt::Write;
        write!(styled, "hello").unwrap();
        let output = CliRunOutput::new(Vec::<u8>::new());
        let rendered = output.render_styled(&styled);
        assert!(!rendered.contains('\u{1b}'));
        assert_eq!(rendered, "hello");
    }

    /// P1：[CliRunOutput::print_styled] 委托 render_styled，写入 color-aware 文本
    /// 条件：force_color 关闭，print_styled 一段带色 StyledStr
    /// 断言：writer 内容为纯文本 + 换行，不含 ANSI 转义
    #[test]
    fn print_styled_writes_color_aware_text() {
        use std::fmt::Write;
        let mut styled = clap::builder::StyledStr::new();
        write!(styled, "world").unwrap();
        let buf = SharedBuf::new();
        let output = CliRunOutput::new(buf.clone());
        output.print_styled(&styled);
        assert_eq!(buf.contents(), "world\n");
        assert!(!buf.contents().contains('\u{1b}'));
    }

    // ── Client::run() ──

    /// P1：[Client::run] 返回 CliRun 实例
    /// 条件：调用 client.run(vec!["test"])
    /// 断言：返回值类型实现了 IntoFuture
    #[test]
    fn run_returns_cli_run() {
        // 验证 CliRun 实现了 IntoFuture
        fn assert_into_future<T: std::future::IntoFuture>(_: &T) {}
        let client = build_isolated_client();
        let cli_run = client.run(vec!["test".into()]);
        assert_into_future(&cli_run);
    }

    // ── Path overrides ──

    /// P0：[CliRun::get_home_dir] 未设置覆盖时返回 client 的 home_dir
    /// 条件：不调用 .home_dir()
    /// 断言：get_home_dir() 等于 client.home_dir()
    #[test]
    fn get_home_dir_defaults_to_client() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert_eq!(run.get_home_dir(), client.home_dir());
    }

    /// P0：[CliRun::get_tmp_dir] 未设置覆盖时返回 client 的 tmp_dir
    /// 条件：不调用 .tmp_dir()
    /// 断言：get_tmp_dir() 等于 client.tmp_dir()
    #[test]
    fn get_tmp_dir_defaults_to_client() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert_eq!(run.get_tmp_dir(), client.tmp_dir());
    }

    /// P0：[CliRun::home_dir] 设置覆盖后 get_home_dir 返回覆盖值
    /// 条件：调用 .home_dir("/custom/home")
    /// 断言：get_home_dir() 返回 "/custom/home"
    #[test]
    fn home_dir_override_takes_precedence() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).home_dir("/custom/home");
        assert_eq!(run.get_home_dir(), Path::new("/custom/home"));
    }

    /// P0：[CliRun::tmp_dir] 设置覆盖后 get_tmp_dir 返回覆盖值
    /// 条件：调用 .tmp_dir("/custom/tmp")
    /// 断言：get_tmp_dir() 返回 "/custom/tmp"
    #[test]
    fn tmp_dir_override_takes_precedence() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).tmp_dir("/custom/tmp");
        assert_eq!(run.get_tmp_dir(), Path::new("/custom/tmp"));
    }

    /// P1：[CliRun::get_cache_dir] 派生自 get_home_dir 的覆盖值
    /// 条件：调用 .home_dir("/custom/home")
    /// 断言：get_cache_dir() 返回 "/custom/home/cache"
    #[test]
    fn get_cache_dir_derived_from_overridden_home() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).home_dir("/custom/home");
        assert_eq!(run.get_cache_dir(), PathBuf::from("/custom/home/cache"));
    }

    /// P1：[CliRun::get_request_storage_dir] 派生自 get_tmp_dir 的覆盖值
    /// 条件：调用 .tmp_dir("/custom/tmp")
    /// 断言：get_request_storage_dir() 返回 "/custom/tmp/requests"
    #[test]
    fn get_request_storage_dir_derived_from_overridden_tmp() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).tmp_dir("/custom/tmp");
        assert_eq!(
            run.get_request_storage_dir(),
            PathBuf::from("/custom/tmp/requests")
        );
    }

    /// P1：[CliRun::get_cache_dir] 未覆盖时派生自 client 的 home_dir
    /// 条件：不调用 .home_dir()
    /// 断言：get_cache_dir() 等于 client.cache_dir()
    #[test]
    fn get_cache_dir_defaults_to_client_derived() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert_eq!(run.get_cache_dir(), client.cache_dir());
    }

    /// P1：[CliRun::get_request_storage_dir] 未覆盖时派生自 client 的 tmp_dir
    /// 条件：不调用 .tmp_dir()
    /// 断言：get_request_storage_dir() 等于 client.request_storage_dir()
    #[test]
    fn get_request_storage_dir_defaults_to_client_derived() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert_eq!(run.get_request_storage_dir(), client.request_storage_dir());
    }

    // ── timeout（每笔独立请求超时） ──

    /// P0：[CliRun::timeout] 链式 setter 把 Duration 写入 self.timeout
    /// 条件：调用 .timeout(Duration::from_secs(7))
    /// 断言：get_timeout() 返回 Some(7s)
    #[test]
    fn timeout_setter_writes_field() {
        let client = build_isolated_client();
        let run = client
            .run(vec!["test".into()])
            .timeout(std::time::Duration::from_secs(7));
        assert_eq!(run.get_timeout(), Some(std::time::Duration::from_secs(7)));
    }

    /// P0：[CliRun::get_timeout] 未调用 .timeout() 时为 None
    /// 条件：构造 CliRun 后不设置 timeout
    /// 断言：get_timeout() 返回 None
    #[test]
    fn timeout_default_none() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert!(run.get_timeout().is_none());
    }

    /// P1：[CliRun::timeout] 与 .header() / .home_dir() 等其他 setter 可链式叠加
    /// 条件：先 .timeout(5s)，再 .header("x-a", "1")，再 .home_dir("/h")
    /// 断言：timeout / headers / home_dir 各自正确生效
    #[test]
    fn timeout_chains_with_other_setters() {
        let client = build_isolated_client();
        let run = client
            .run(vec!["test".into()])
            .timeout(std::time::Duration::from_secs(5))
            .header("x-a", "1")
            .home_dir("/h");
        assert_eq!(run.get_timeout(), Some(std::time::Duration::from_secs(5)));
        let hdrs = run.get_headers();
        assert_eq!(hdrs.get("x-a").unwrap().to_str().unwrap(), "1");
        assert_eq!(run.get_home_dir(), Path::new("/h"));
    }

    /// P1：[CliRun::timeout] 多次调用后写覆盖前写
    /// 条件：先 .timeout(3s)，再 .timeout(11s)
    /// 断言：get_timeout() 返回 Some(11s)
    #[test]
    fn timeout_last_one_wins() {
        let client = build_isolated_client();
        let run = client
            .run(vec!["test".into()])
            .timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(11));
        assert_eq!(run.get_timeout(), Some(std::time::Duration::from_secs(11)));
    }

    // ── extensions（run 级写入 options；默认层归 transport 构建期） ──

    /// P1：[CliRun::extension] run 级扩展值写入 options
    /// 条件：client.run() 后调用 .extension(RunExt(2))
    /// 断言：cli_run.options.extensions.get::<RunExt>() 为 Some(2)
    #[test]
    fn run_level_extension_written_to_options() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]).extension(RunExt(2));
        assert_eq!(run.options.extensions.get::<RunExt>(), Some(&RunExt(2)));
    }

    /// P1：[Client::run] 未注入扩展时袋为空
    /// 条件：build_isolated_client()（无扩展）
    /// 断言：cli_run.options.extensions.is_empty() 为 true
    #[test]
    fn run_extensions_default_empty() {
        let client = build_isolated_client();
        let run = client.run(vec!["test".into()]);
        assert!(run.options.extensions.is_empty());
    }

    // ── extensions 端到端：CliRun → 全部请求 → 后端 execute ──

    /// 测试夹具：合法的服务 schema JSON（detail 缓存与 discovery 响应共用）。
    fn svc_schema_json() -> serde_json::Value {
        serde_json::json!({
            "description": "test service",
            "base_url": "https://test.example.com/",
            "methods": { "list": { "http_method": "GET", "path": "/list" } },
            "resources": {}
        })
    }

    /// 测试夹具：记录每次 execute 收到的 RequestOptions 的捕获型后端。
    ///
    /// 对 payload 含 `"service"` 键的 discovery 请求返回合法 schema，
    /// 其余（业务）请求返回空 JSON 对象。
    #[derive(Debug)]
    struct CaptureBackend {
        captured: std::sync::Arc<std::sync::Mutex<Vec<wecom_transport::RequestOptions>>>,
    }

    impl wecom_transport::TransportBackend for CaptureBackend {
        fn execute<'a>(
            &'a self,
            _endpoint: std::borrow::Cow<'a, wecom_transport::Endpoint>,
            payload: wecom_transport::HttpRequestPayload<'a>,
            options: wecom_transport::RequestOptions,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<
                            wecom_transport::TransportResponse,
                            wecom_transport::Error,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.captured.lock().unwrap().push(options);
            let is_discovery = matches!(
                &payload,
                wecom_transport::HttpRequestPayload::Json(v) if v.get("service").is_some()
            );
            let result = if is_discovery {
                svc_schema_json()
            } else {
                serde_json::json!({})
            };
            Box::pin(async {
                Ok(wecom_transport::TransportResponse::Json(
                    wecom_transport::ExecuteOutput {
                        result,
                        extra: indexmap::IndexMap::new(),
                    },
                ))
            })
        }
    }

    /// 测试夹具：播种 catalog 缓存（list_services 无需网络）。
    fn seed_run_catalog_cache(root: &std::path::Path, service: &str) {
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        #[allow(clippy::disallowed_methods)] // Test helper: write fixture outside sandbox
        std::fs::write(
            cache_dir.join("catalog.json"),
            serde_json::to_string(&serde_json::json!({ "items": [{ "name": service }] })).unwrap(),
        )
        .unwrap();
    }

    /// 测试夹具：播种服务 detail 缓存（schema 拉取无需网络）。
    fn seed_run_detail_cache(root: &std::path::Path, service: &str) {
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let detail = cache_dir.join(format!(
            "service_{}.json",
            crate::fs::sanitize_filename(service)
        ));
        #[allow(clippy::disallowed_methods)] // Test helper: write fixture outside sandbox
        std::fs::write(detail, serde_json::to_string(&svc_schema_json()).unwrap()).unwrap();
    }

    /// 测试夹具：捕获型后端 + 共享 captured 缓冲 + client 构造。
    ///
    /// `seed_detail` 为 false 时仅播种 catalog，schema 拉取会真实经过后端。
    fn build_capture_client(
        ext: RunExt,
        seed_detail: bool,
    ) -> (
        Client,
        std::sync::Arc<std::sync::Mutex<Vec<wecom_transport::RequestOptions>>>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        seed_run_catalog_cache(&root, "svc");
        if seed_detail {
            seed_run_detail_cache(&root, "svc");
        }
        let captured = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let backend = CaptureBackend {
            captured: captured.clone(),
        };
        let client = Client::builder()
            .home_dir(&root)
            .cwd(&root)
            .transport(
                wecom_transport::TransportBuilder::new(backend)
                    .extension(ext)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        (client, captured)
    }

    /// P0：[CliRun → TransportRequest] transport 默认扩展袋经业务调用到达后端 execute
    /// 条件：捕获型后端 + 已播种缓存；TransportBuilder 级 RunExt(1)，run 级不再设置
    /// 断言：execute 恰好收到 1 次请求，options.extensions 含 RunExt(1)
    #[tokio::test]
    async fn run_transport_extensions_reach_backend_execute() {
        let (client, captured) = build_capture_client(RunExt(1), true);
        client
            .run(vec!["wecom".into(), "svc".into(), "list".into()])
            .execute()
            .await
            .unwrap();
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "expect exactly one business request");
        assert_eq!(captured[0].extensions.get::<RunExt>(), Some(&RunExt(1)));
    }

    /// P0：[CliRun → TransportRequest] run 级 options 全字段覆盖后到达后端 execute
    /// 条件：捕获型后端 + 已播种缓存；transport 级 RunExt(1)；run 级
    ///       .extension(RunExt(2)) + .header("x-run-scope","yes") + .timeout(7s)
    /// 断言：execute 收到的 options 中扩展袋 / header / timeout 全部为 run 级值
    #[tokio::test]
    async fn run_level_options_override_reaches_backend_execute() {
        let (client, captured) = build_capture_client(RunExt(1), true);
        client
            .run(vec!["wecom".into(), "svc".into(), "list".into()])
            .extension(RunExt(2))
            .header("x-run-scope", "yes")
            .timeout(std::time::Duration::from_secs(7))
            .execute()
            .await
            .unwrap();
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "expect exactly one business request");
        let got = &captured[0];
        assert_eq!(got.extensions.get::<RunExt>(), Some(&RunExt(2)));
        assert_eq!(got.wire.headers.get("x-run-scope").unwrap(), "yes");
        assert_eq!(got.wire.timeout, Some(std::time::Duration::from_secs(7)));
    }

    /// P0：[CliRun → discovery] run 级 options 对 run 触发的 discovery 请求同样生效
    /// 条件：捕获型后端；仅播种 catalog（detail 拉取真实经过后端）；transport 级
    ///       RunExt(1)；run 级 .extension(RunExt(2)) + .header("x-run-scope","yes")
    /// 断言：恰好 2 次请求（discovery detail + 业务），两次请求的 options 均含
    ///       RunExt(2) 与 x-run-scope 头
    #[tokio::test]
    async fn run_level_options_reach_discovery_fetch() {
        let (client, captured) = build_capture_client(RunExt(1), false);
        client
            .run(vec!["wecom".into(), "svc".into(), "list".into()])
            .extension(RunExt(2))
            .header("x-run-scope", "yes")
            .execute()
            .await
            .unwrap();
        let captured = captured.lock().unwrap();
        assert_eq!(
            captured.len(),
            2,
            "expect discovery detail fetch + business request"
        );
        for (i, got) in captured.iter().enumerate() {
            assert_eq!(
                got.extensions.get::<RunExt>(),
                Some(&RunExt(2)),
                "request #{i} 扩展袋应为 run 级值"
            );
            assert_eq!(
                got.wire.headers.get("x-run-scope").unwrap(),
                "yes",
                "request #{i} header 应为 run 级值"
            );
        }
    }

    /// 测试夹具：run 扩展袋用例。
    #[derive(Debug, PartialEq)]
    struct RunExt(u32);
}
