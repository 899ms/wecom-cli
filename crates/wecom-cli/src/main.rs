mod auth;
mod browser;
mod cmd;
mod config;
mod env;
mod error;
mod logging;
mod telemetry;
mod transport;

use error::Error;
use tracing::Instrument;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let root_span = logging::init_logging();

    // 挂载 json repair 提示监听：repair 成功时向 stderr 输出修复前后 JSON。
    let scope = wecom_transport::telemetry::CaptureScope::attach(&root_span);
    telemetry::install_json_repair_listener(&scope);

    let run = async {
        let cfg = config::load_config_file(&config::default_config_path())?.unwrap_or_default();

        let builder = wecom::Client::builder();
        let builder = config::apply_config(builder, &cfg)?;
        let builder = builder.endpoint_catalog(transport::endpoint_catalog());

        let transport = transport::build(&cfg).await?.with_extension(cfg);

        let client = builder
            .transport(transport)
            .bin_name(env!("CARGO_BIN_NAME"))
            .command(cmd::auth::custom_command())
            .build()?;

        client.run(std::env::args().collect()).await
    };

    if let Err(err) = run.instrument(root_span).await {
        println!("{}", err.render());
        // 命令未找到时提示更新 SKILL（stderr，不污染 stdout）。
        if is_subcommand_not_found(&err) {
            eprintln!();
            eprintln!("{SKILL_HINT}");
        }
        std::process::exit(err.exit_code());
    }
}

/// 命令未找到时的提示文案。
const SKILL_HINT: &str = "该接口不存在，可能未接口命令错误，请更新到最新 skill 后重试";

/// 是否为"命令未找到"类错误。
fn is_subcommand_not_found(err: &wecom::Error) -> bool {
    match err {
        wecom::Error::CliOutput {
            source: Some(e), ..
        } => e.kind() == clap::error::ErrorKind::InvalidSubcommand,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_output(kind: clap::error::ErrorKind) -> wecom::Error {
        wecom::Error::CliOutput {
            code: 2,
            message: String::new(),
            source: Some(clap::Error::raw(kind, "boom")),
        }
    }

    /// unknown subcommand → 命中提示
    #[test]
    fn invalid_subcommand_hits() {
        assert!(is_subcommand_not_found(&cli_output(
            clap::error::ErrorKind::InvalidSubcommand
        )));
    }

    /// 其它 clap 错误（如参数缺失）→ 不命中
    #[test]
    fn other_clap_kind_misses() {
        assert!(!is_subcommand_not_found(&cli_output(
            clap::error::ErrorKind::MissingRequiredArgument
        )));
    }

    /// 非 clap 来源（如后端 10021 用法错误）→ 不命中
    #[test]
    fn cli_output_without_source_misses() {
        assert!(!is_subcommand_not_found(&wecom::Error::CliOutput {
            code: 2,
            message: String::new(),
            source: None,
        }));
    }

    /// 其它错误变体 → 不命中
    #[test]
    fn other_errors_miss() {
        assert!(!is_subcommand_not_found(&wecom::Error::Validation(
            "x".into()
        )));
    }
}
