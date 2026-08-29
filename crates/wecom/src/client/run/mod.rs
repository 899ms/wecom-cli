//! CLI run pipeline: [`CliRun`] run context, its output configuration,
//! clap command-tree execution, and parse-error handling.
//!
//! Split by responsibility:
//! - [`output`] — [`CliRunOutput`] / [`Writer`] / extra-data callback
//! - [`execute`] — root command-tree build + subcommand dispatch
//! - [`parse_error`] — clap error handling + relaxed re-parse path resolution

mod execute;
mod output;
mod parse_error;
#[cfg(test)]
mod tests;

use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use clap::ArgMatches;
use indexmap::IndexMap;
use output::ExtraDataCallback;
pub use output::{CliRunOutput, Writer};

use super::Client;
use crate::{Error, Result, fs};

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
    /// Shared between `--help` rendering ([`crate::service::handler`]) and the CLI
    /// parse-error path ([`execute`](Self::execute)).
    pub(crate) fn render_help_message(&self, fallback: &clap::builder::StyledStr) -> String {
        self.output.render_styled(fallback)
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
