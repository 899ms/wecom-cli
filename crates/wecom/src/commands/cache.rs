use std::path::Path;

use clap::{ArgMatches, Command, FromArgMatches, Subcommand};
use serde_json::json;

use crate::{CliRun, CliRunOutput, Error, Result, fs};

#[derive(Subcommand)]
#[command(subcommand_required = true)]
pub enum CacheCmds {
    /// 查看服务发现缓存状态
    Status,

    /// 清除所有服务发现缓存文件
    Clear,
}

pub fn build_cache_cmd() -> Command {
    CacheCmds::augment_subcommands(Command::new("cache")).hide(true)
}

pub async fn handle_cache_cmd(run: &CliRun<'_>, matches: &ArgMatches) -> Result<()> {
    let output = run.get_output();
    let cache_dir = run.get_cache_dir();

    // cache 命令使用独立的 Fs，仅放开 cache 目录的读写权限，
    // 不受 CliRun 全局沙箱（readable/writable_dirs）限制。
    let cache_fs = fs::Fs::new_with_permissions(
        &cache_dir,
        Some(&[cache_dir.as_path()]),
        Some(&[cache_dir.as_path()]),
    );

    match CacheCmds::from_arg_matches(matches) {
        Ok(CacheCmds::Status) => handle_cache_status(&cache_fs, &cache_dir, output).await,
        Ok(CacheCmds::Clear) => handle_cache_clear(&cache_fs, &cache_dir, output).await,
        _ => Err(Error::Other("Unknown cache subcommand".into())),
    }
}

/// 列出当前缓存目录下所有文件及其修改时间。
#[tracing::instrument(level = "debug", name = "cache.status", skip_all)]
async fn handle_cache_status(fs: &fs::Fs, cache_dir: &Path, output: &CliRunOutput) -> Result<()> {
    tracing::info!(cache_dir = %cache_dir.display(), "listing cache status");

    let files: Vec<_> = fs
        .list_dir(cache_dir)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_file())
        .collect();

    let mut entries: Vec<_> = Vec::new();
    for path in &files {
        if let Ok(metadata) = fs.metadata(path).await
            && let Ok(modified) = metadata.modified()
            && let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            entries.push(serde_json::json!({
                "file": path.file_name().unwrap_or_default().to_string_lossy(),
                "update_time": dur.as_secs(),
            }));
        }
    }

    output.print(&serde_json::to_string_pretty(&entries).unwrap_or_default());
    Ok(())
}

/// 清除缓存目录下所有文件。
#[tracing::instrument(level = "debug", name = "cache.clear", skip_all)]
async fn handle_cache_clear(fs: &fs::Fs, cache_dir: &Path, output: &CliRunOutput) -> Result<()> {
    tracing::info!(cache_dir = %cache_dir.display(), "clearing cache");

    let files: Vec<_> = fs
        .list_dir(cache_dir)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    let mut removed = Vec::new();

    for path in &files {
        match fs.remove_file(path).await {
            Ok(()) => {
                removed.push(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
            }
            Err(e) => {
                tracing::info!(path = %path.display(), error = %e, "Failed to remove cache file");
            }
        }
    }

    let output_val = if removed.is_empty() {
        json!({
            "status": "success",
            "message": "没有需要清除的缓存文件。",
        })
    } else {
        json!({
            "status": "success",
            "message": format!("已清除 {} 个缓存文件。", removed.len()),
            "removed": removed,
        })
    };

    output.print(&serde_json::to_string_pretty(&output_val).unwrap_or_default());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：cache（缓存命令处理）
    //!
    //! ### 关键接口
    //! - [handle_cache_status] — 列出缓存目录中所有文件及其修改时间
    //! - [handle_cache_clear] — 清除所有缓存文件并返回统计信息
    //!
    //! ### 关键分支与异常路径
    //! - handle_cache_status：空目录返回空数组；有文件时返回文件列表
    //! - handle_cache_clear：空目录返回提示信息；有文件时删除并返回统计
    //! - handle_cache_cmd：使用独立 Fs（仅以 cache_dir 为 root），
    //!   即使 CliRun 全局沙箱不含 cache_dir 也能正常工作
    //!
    //! ### 上下游交互
    //! - 上游：[commands::handle_cache_cmd]（接受 &CliRun）调用本模块
    //! - 下游：本模块内部构造独立 [fs::Fs]（仅以 cache_dir 为 root），
    //!   不使用 [CliRun::fs]

    use std::fs as stdfs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use assert_json_diff::assert_json_eq;
    use tempfile::TempDir;

    use super::*;
    use crate::Client;

    /// A cloneable buffer for capturing output.
    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn build_client(home: &std::path::Path) -> Client {
        Client::builder()
            .home_dir(home)
            .tmp_dir(home)
            .build()
            .unwrap()
    }

    // ── handle_cache_status ──

    /// 构造一个仅以 cache_dir 为读写 root 的独立 Fs，等价于
    /// `handle_cache_cmd` 内部的 fs。
    fn build_cache_fs(cache_dir: &std::path::Path) -> fs::Fs {
        fs::Fs::new_with_permissions(cache_dir, Some(&[cache_dir]), Some(&[cache_dir]))
    }

    /// P0：[handle_cache_status] 在空缓存目录下返回空数组
    /// 条件：缓存目录已创建但无任何文件
    /// 断言：输出为合法 JSON 空数组
    #[tokio::test]
    async fn cache_status_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let cache_dir = tmp.path().join("cache");
        let output = CliRunOutput::new(buf.clone());

        stdfs::create_dir_all(&cache_dir).unwrap();
        let cache_fs = build_cache_fs(&cache_dir);

        let result = handle_cache_status(&cache_fs, &cache_dir, &output).await;
        assert!(result.is_ok());

        let output = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed.as_array().unwrap().is_empty());
    }

    /// P1：[handle_cache_status] 缓存状态查询能列出已有文件
    /// 条件：缓存目录中包含 catalog.json 文件
    /// 断言：输出 JSON 数组长度为 1，且包含 "catalog.json" 文件名
    #[tokio::test]
    async fn cache_status_with_files() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let cache_dir = tmp.path().join("cache");
        let output = CliRunOutput::new(buf.clone());

        stdfs::create_dir_all(&cache_dir).unwrap();
        stdfs::write(cache_dir.join("catalog.json"), "{}").unwrap();
        let cache_fs = build_cache_fs(&cache_dir);

        let result = handle_cache_status(&cache_fs, &cache_dir, &output).await;
        assert!(result.is_ok());

        let output = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_json_eq!(parsed[0]["file"], serde_json::json!("catalog.json"));
    }

    // ── handle_cache_clear ──

    /// P1：[handle_cache_clear] 清除空缓存目录时返回提示信息
    /// 条件：缓存目录存在但不包含任何文件
    /// 断言：输出中包含 "没有需要清除的缓存文件" 提示文本
    #[tokio::test]
    async fn cache_clear_empty() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let cache_dir = tmp.path().join("cache");
        let output = CliRunOutput::new(buf.clone());

        stdfs::create_dir_all(&cache_dir).unwrap();
        let cache_fs = build_cache_fs(&cache_dir);

        let result = handle_cache_clear(&cache_fs, &cache_dir, &output).await;
        assert!(result.is_ok());

        let output = buf.contents();
        assert!(output.contains("没有需要清除的缓存文件"));
    }

    /// P1：[handle_cache_clear] 清除缓存目录时删除所有文件并返回统计
    /// 条件：缓存目录中包含 old.json 和 stale.json 两个文件
    /// 断言：目录变空且输出包含 "已清除 2 个缓存文件"
    #[tokio::test]
    async fn cache_clear_removes_files() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let cache_dir = tmp.path().join("cache");
        let output = CliRunOutput::new(buf.clone());

        stdfs::create_dir_all(&cache_dir).unwrap();
        stdfs::write(cache_dir.join("old.json"), "x").unwrap();
        stdfs::write(cache_dir.join("stale.json"), "y").unwrap();
        let cache_fs = build_cache_fs(&cache_dir);

        let result = handle_cache_clear(&cache_fs, &cache_dir, &output).await;
        assert!(result.is_ok());

        // Files should be gone
        let remaining: Vec<_> = cache_fs
            .list_dir(&cache_dir)
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
        assert!(remaining.is_empty());

        let output = buf.contents();
        assert!(output.contains("已清除 2 个缓存文件"));
    }

    // ── handle_cache_cmd（独立 fs 行为）──

    /// P0：[handle_cache_cmd] 即使 CliRun 全局沙箱不含 cache_dir，
    /// `cache status` 仍可正常列出 cache_dir 中的文件。
    /// 条件：CliRun 设置 readable/writable_dirs 为另一个无关目录；
    ///       cache_dir 内有一个 entry.json 文件
    /// 断言：cache status 返回成功，输出 JSON 数组包含 entry.json
    #[tokio::test]
    async fn cache_cmd_uses_independent_fs_for_status() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let client = build_client(tmp.path());
        let cache_dir = client.cache_dir();
        stdfs::create_dir_all(&cache_dir).unwrap();
        stdfs::write(cache_dir.join("entry.json"), "{}").unwrap();

        // 一个完全不含 cache_dir 的目录，作为 CliRun 全局沙箱 root
        let unrelated = TempDir::new().unwrap();
        let unrelated_path: PathBuf = unrelated.path().to_path_buf();

        let cache_matches = build_cache_cmd().get_matches_from(["cache", "status"]);
        let run = client
            .run(vec!["test".into()])
            .output(CliRunOutput::new(buf.clone()))
            .readable_dirs(vec![unrelated_path.clone()])
            .writable_dirs(vec![unrelated_path]);
        let result = handle_cache_cmd(&run, &cache_matches).await;
        assert!(result.is_ok(), "cache status failed: {result:?}");

        let output = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_json_eq!(arr[0]["file"], serde_json::json!("entry.json"));
    }

    /// P0：[handle_cache_cmd] 即使 CliRun 全局沙箱不含 cache_dir，
    /// `cache clear` 仍可正常删除 cache_dir 中的文件。
    /// 条件：CliRun 设置 readable/writable_dirs 为另一个无关目录；
    ///       cache_dir 内有 a.json / b.json 两个文件
    /// 断言：clear 成功，cache_dir 内无文件，输出包含 "已清除 2 个缓存文件"
    #[tokio::test]
    async fn cache_cmd_uses_independent_fs_for_clear() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let client = build_client(tmp.path());
        let cache_dir = client.cache_dir();
        stdfs::create_dir_all(&cache_dir).unwrap();
        stdfs::write(cache_dir.join("a.json"), "x").unwrap();
        stdfs::write(cache_dir.join("b.json"), "y").unwrap();

        let unrelated = TempDir::new().unwrap();
        let unrelated_path: PathBuf = unrelated.path().to_path_buf();

        let cache_matches = build_cache_cmd().get_matches_from(["cache", "clear"]);
        let run = client
            .run(vec!["test".into()])
            .output(CliRunOutput::new(buf.clone()))
            .readable_dirs(vec![unrelated_path.clone()])
            .writable_dirs(vec![unrelated_path]);
        let result = handle_cache_cmd(&run, &cache_matches).await;
        assert!(result.is_ok(), "cache clear failed: {result:?}");

        // 文件被清理
        assert!(stdfs::read_dir(&cache_dir).unwrap().next().is_none());

        let output = buf.contents();
        assert!(output.contains("已清除 2 个缓存文件"));
    }

    // ── build_cache_cmd ──

    /// P1：[build_cache_cmd] 构造 cache 子命令并隐藏
    /// 条件：调用 build_cache_cmd()
    /// 断言：命令名为 "cache" 且 is_hide_set() 为 true
    #[test]
    fn build_cache_cmd_returns_command() {
        let cmd = build_cache_cmd();
        assert_eq!(cmd.get_name(), "cache");
        // cache 命令对用户隐藏
        assert!(cmd.is_hide_set());
    }

    // ── handle_cache_cmd 错误路径 ──

    /// P2：[handle_cache_cmd] 未知子命令返回 Error::Other
    /// 条件：传入非 status/clear 的子命令匹配（空 ArgMatches）
    /// 断言：handle_cache_cmd() 返回 Err
    #[tokio::test]
    async fn cache_cmd_unknown_subcommand_returns_error() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let client = build_client(tmp.path());
        let run = client
            .run(vec!["test".into()])
            .output(CliRunOutput::new(buf.clone()));
        // 构造一个空的 ArgMatches（不会匹配任何子命令）
        let empty_matches = clap::Command::new("dummy").get_matches_from(Vec::<&str>::new());
        let result = handle_cache_cmd(&run, &empty_matches).await;
        assert!(result.is_err());
    }

    // ── handle_cache_clear 移除文件失败 ──

    /// P2：[handle_cache_clear] 移除文件失败不中断清除流程
    /// 条件：缓存目录中存在一个文件，但移除时模拟失败场景
    /// 断言：函数成功返回（Err 分支被静默吞掉）
    #[tokio::test]
    async fn cache_clear_remove_file_error_does_not_abort() {
        let tmp = TempDir::new().unwrap();
        let buf = SharedBuf::new();
        let cache_dir = tmp.path().join("cache");
        let output = CliRunOutput::new(buf.clone());

        stdfs::create_dir_all(&cache_dir).unwrap();
        let file = cache_dir.join("locked.json");
        stdfs::write(&file, "x").unwrap();
        // Remove write permission from cache dir to trigger error on list/remove
        let mut perms = stdfs::metadata(&cache_dir).unwrap().permissions();
        perms.set_mode(0o500); // read-only dir: r-x------
        stdfs::set_permissions(&cache_dir, perms).unwrap();

        // Use unrestricted Fs so the permission error comes from the actual OS call
        let cache_fs = fs::Fs::new(&cache_dir);
        let result = handle_cache_clear(&cache_fs, &cache_dir, &output).await;
        assert!(result.is_ok());

        // Restore permissions for cleanup
        let mut perms = stdfs::metadata(&cache_dir).unwrap().permissions();
        perms.set_mode(0o700); // rwx------
        stdfs::set_permissions(&cache_dir, perms).unwrap();
    }
}
