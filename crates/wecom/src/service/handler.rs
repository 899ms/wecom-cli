use std::path::PathBuf;

use clap::{ArgMatches, Command, FromArgMatches};

use super::RunOptions;
use super::command::{self, HelperCmdArgs, MethodCmdArgs, ServiceCmdArgs};
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
    matches: &ArgMatches,
    cmd: &Command,
) -> Result<()> {
    let client = run.get_client();
    let output = run.get_output();

    let args = ServiceCmdArgs::from_arg_matches(matches)
        .map_err(|e| Error::Other(format!("参数解析错误: {e:#}").into()))?;

    let svc = client.service_with_options(name, run.get_options()).await?;

    // service --doc
    if args.doc == Some(true) {
        tracing::info!(service = %name, "display service doc");
        output.print_styled(&svc.doc());
        return Ok(());
    }

    // service --schema
    if args.schema == Some(true) {
        tracing::info!(service = %name, "display service schema");
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

    // Lookup method BEFORE parsing MethodCmdArgs so that unknown subcommands
    // (captured as external subcommands via `.allow_external_subcommands(true)`)
    // fail with a skill-aware `Error::CliOutput` instead of panicking in
    // `from_arg_matches` over undefined arg ids.
    let method = svc.method(&cmd_path_strs)?;

    let mut args = MethodCmdArgs::from_arg_matches(cmd_matches)
        .map_err(|e| Error::Other(format!("参数解析错误: {e:#}").into()))?;

    let mut payload =
        command::assemble_payload(&mut args, method.request_schema().as_ref(), cmd_matches)
            .inspect_err(|e| tracing::error!(error = %e, "请求参数解析失败"))?;

    // ── 分派（装配后，schema 第一优先级）──
    if args.help == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, "display method help");
        output.print(&run.render_leaf_help(cmd, &full_path, None));
        return Ok(());
    }

    // --schema
    if args.schema == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, "display method schema");
        output.print(&serde_json::to_string_pretty(&method.schema()).unwrap_or_default());
        return Ok(());
    }

    // --doc
    if args.doc == Some(true) {
        tracing::info!(service = %name, method = ?cmd_path_strs, "display method doc");
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

fn get_subcmd_path(matches: &ArgMatches) -> Result<(Vec<String>, &ArgMatches)> {
    let mut path = vec![];
    let mut matches = matches;
    while let Some((name, sub_matches)) = matches.subcommand() {
        path.push(name.to_string());
        matches = sub_matches;
    }
    Ok((path, matches))
}
