---
"@wecom/cli": major
---

统一输出、错误与环境变量约定

- 错误改为以结构化 JSON 输出到 **stdout**（`{"error": {"type", "code", "message"}}`），日志与提示信息一律走 stderr；CLI 自身错误码段为 893000–893299，后台 errcode 原样透传。
- 明确退出码约定：`0` 成功（含 `--help` / `--version`），`1` 运行时错误，`2` 用法错误（后台返回用法类错误码时渲染当前命令 help 并以 2 退出）。
- `--version` 输出格式改为 `wecom-cli <version> (<distribution> <RFC 3339 构建时间> <git_commit_id>)`。
- `WECOM_CLI_LOG_FILE` 移除，改为 `WECOM_CLI_LOG_DIR`：值为目录，JSON Lines 日志按天写入 `<dir>/ww.log.<日期>`（UTC+8）。
- 新增 `WECOM_CLI_ADDITIONAL_HEADERS`（及 `WECOM_CLI_ADDITIONAL_HEADERS_*` 后缀形式）注入额外请求头。
- 新增可选配置文件 `<config_dir>/config.json`（`headers` / `tmp_dir`），环境变量优先级高于配置文件。

迁移指引：解析 stderr 文本错误的脚本请改为解析 stdout 的 JSON 错误；使用 `WECOM_CLI_LOG_FILE` 的环境请改用 `WECOM_CLI_LOG_DIR` 并传入目录路径。
