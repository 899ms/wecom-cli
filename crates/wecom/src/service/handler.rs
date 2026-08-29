use std::path::PathBuf;

use clap::{ArgMatches, Command, FromArgMatches};

use super::command::{self, HelperCmdArgs, MethodCmdArgs, ServiceCmdArgs};
use super::{RunOptions, remote_doc};
use crate::client::CliRun;
use crate::{Error, Result};

#[tracing::instrument(
    level = "info",
    name = "service.handle",
    skip_all,
    fields(service = %name),
)]
pub async fn handle_service_cmd(
    run: &CliRun<'_>,
    name: &str,
    input_name: &str,
    matches: &ArgMatches,
    cmd: &Command,
) -> Result<()> {
    let client = run.get_client();
    let output = run.get_output();

    let args = ServiceCmdArgs::from_arg_matches(matches)
        .map_err(|e| Error::Other(format!("参数解析错误: {e:#}").into()))?;

    // name 来自 clap matches（别名已归一化为规范名）；input_name 是用户原始
    // 输入的 argv token，保留给 method() 的 method_alias 遥测还原 input。
    let svc = client
        .service_with_options(name, run.get_options())
        .await?
        .with_input_name(input_name);

    // service --doc
    if args.doc == Some(true) {
        tracing::info!(service = %name, remote = svc.remote_doc_id().is_some(), "display service doc");
        if let Some(id) = svc.remote_doc_id() {
            return print_remote_doc(run, id, "doc").await;
        }
        output.print_styled(&svc.doc());
        return Ok(());
    }

    // service --schema
    if args.schema == Some(true) {
        tracing::info!(service = %name, remote = svc.remote_doc_id().is_some(), "display service schema");
        if let Some(id) = svc.remote_doc_id() {
            return print_remote_doc(run, id, "schema").await;
        }
        output.print(&serde_json::to_string_pretty(&svc.schema()).unwrap_or_default());
        return Ok(());
    }

    let (cmd_path, cmd_matches) = get_subcmd_path(matches)?;
    let cmd_path_strs: Vec<_> = cmd_path.iter().map(|s| s.as_str()).collect();
    // Full command path including the service name, e.g. ["media", "+download"].
    let full_path: Vec<&str> = std::iter::once(name)
        .chain(cmd_path_strs.iter().copied())
        .collect();

    // +helper
    if let Some(helper) = svc.helper(&cmd_path_strs) {
        tracing::info!(service = %name, helper = cmd_path_strs.join(" "), "routing to helper");

        let meta = helper.about();
        let mut args = HelperCmdArgs::from_arg_matches(cmd_matches)
            .map_err(|e| Error::Other(format!("Unexpected argument error: {e:#}").into()))?;

        let payload = command::assemble_payload(&mut args, Some(&meta.request), cmd_matches)
            .inspect_err(|e| tracing::error!(error = %e, "请求参数解析失败"))?;

        // ── 分派（装配后，schema 第一优先级）──
        if args.help == Some(true) {
            tracing::info!(service = %name, helper = cmd_path_strs.join(" "), "display helper help");
            output.print(&run.render_leaf_help(cmd, &full_path, None));
            return Ok(());
        }

        // +helper --schema
        if args.schema == Some(true) {
            tracing::info!(service = %name, helper = cmd_path_strs.join(" "), "display helper schema");
            let info = meta.schema_info(&full_path);
            output.print(&serde_json::to_string_pretty(&info).unwrap_or_default());
            return Ok(());
        }

        // +helper --doc
        if args.doc == Some(true) {
            tracing::info!(service = %name, helper = cmd_path_strs.join(" "), "display helper doc");
            output.print_styled(&meta.doc(client.bin_name(), &full_path));
            return Ok(());
        }

        helper.execute(run, payload).await?;
        return Ok(());
    }

    // Look up the method BEFORE parsing `MethodCmdArgs`: the alias rewrite
    // inside `svc.method` must resolve the real method first (payload assembly
    // needs its schema), and `from_arg_matches` must never run against an
    // undefined method. Unknown subcommands are already rejected by clap as
    // `InvalidSubcommand` in `CliRun::execute` and handled via
    // `handle_parse_error` (skill-aware `Error::CliOutput`).
    let method = svc.method(&cmd_path_strs)?;

    let mut args = MethodCmdArgs::from_arg_matches(cmd_matches)
        .map_err(|e| Error::Other(format!("参数解析错误: {e:#}").into()))?;

    let mut payload =
        command::assemble_payload(&mut args, method.request_schema().as_ref(), cmd_matches)
            .inspect_err(|e| tracing::error!(error = %e, "请求参数解析失败"))?;

    // ── 分派（装配后，schema 第一优先级）──
    if args.help == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, remote = method.remote_doc_id().is_some(), "display method help");
        if let Some(id) = method.remote_doc_id() {
            return print_remote_doc(run, id, "help").await;
        }
        output.print(&run.render_leaf_help(cmd, &full_path, None));
        return Ok(());
    }

    // --schema
    if args.schema == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, remote = method.remote_doc_id().is_some(), "display method schema");
        if let Some(id) = method.remote_doc_id() {
            return print_remote_doc(run, id, "schema").await;
        }
        output.print(&serde_json::to_string_pretty(&method.schema()).unwrap_or_default());
        return Ok(());
    }

    // --doc
    if args.doc == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, remote = method.remote_doc_id().is_some(), "display method doc");
        if let Some(id) = method.remote_doc_id() {
            return print_remote_doc(run, id, "doc").await;
        }
        output.print_styled(&method.doc());
        return Ok(());
    }

    // --dry-run
    if args.dry_run == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, "dry run mode");
        let infos = method.preview(&mut payload)?;
        output.print("=== Dry Run ===");
        for info in &infos {
            output.print(&serde_json::to_string_pretty(info).unwrap_or_default());
        }
        return Ok(());
    }

    // ── 实际调用 ─────────────────────────────────────────────────────
    tracing::info!(service = %name, method = ?cmd_path_strs, paged = args.page_count, "executing method");
    method
        .run(RunOptions {
            run,
            payload,
            page_count: args.page_count, // Option<u32>: None = single page, Some(n) = paginate
            page_delay_ms: args.page_delay.unwrap_or(100),
            output_path: args.output.map(PathBuf::from),
            output_dir: args.output_dir.map(PathBuf::from),
        })
        .await
}

// ── 辅助函数 ────────────────────────────────────────────────

/// Fetch the remotely generated document for the node identified by `id` and
/// print it verbatim to stdout. 调用方保证 `remote_doc` 生效（见
/// [crate::service::remote_doc::resolve_node]）。
async fn print_remote_doc(run: &CliRun<'_>, id: &str, doc_type: &str) -> Result<()> {
    let doc = remote_doc::fetch_remote_doc(run, id, doc_type).await?;
    run.get_output().print(&doc);
    Ok(())
}

fn get_subcmd_path(matches: &ArgMatches) -> Result<(Vec<String>, &ArgMatches)> {
    let mut path = vec![];
    let mut matches = matches;
    while let Some((name, sub_matches)) = matches.subcommand() {
        path.push(name.to_string());
        matches = sub_matches;
    }
    Ok((path, matches))
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：handler（service 命令分发）
    //!
    //! ### 关键接口
    //! - [get_subcmd_path] — 从 clap ArgMatches 提取子命令路径
    //!
    //! ### 关键分支与异常路径
    //! - 嵌套子命令 → 逐层提取路径
    //! - 无子命令 → 返回空路径
    //!
    //! ### 上下游交互
    //! - 上游：handle_service_cmd 在路由 helper / method 前解析子命令路径
    //! - 下游：clap::ArgMatches::subcommand

    use super::*;

    /// P0：[get_subcmd_path] 提取嵌套子命令路径
    /// 条件：ArgMatches 含 media → download 两级子命令
    /// 断言：返回 ["media", "download"]
    #[test]
    fn get_subcmd_path_extracts_nested_path() {
        let cmd = Command::new("wecom")
            .subcommand(Command::new("media").subcommand(Command::new("download")));
        let matches = cmd
            .try_get_matches_from(["wecom", "media", "download"])
            .unwrap();
        let (path, _) = get_subcmd_path(&matches).unwrap();
        assert_eq!(path, vec!["media".to_string(), "download".to_string()]);
    }

    /// P1：[get_subcmd_path] 无子命令时返回空路径
    /// 条件：ArgMatches 不含任何子命令
    /// 断言：返回空 Vec
    #[test]
    fn get_subcmd_path_empty_when_no_subcommand() {
        let cmd = Command::new("wecom");
        let matches = cmd.try_get_matches_from(["wecom"]).unwrap();
        let (path, _) = get_subcmd_path(&matches).unwrap();
        assert!(path.is_empty());
    }
}
