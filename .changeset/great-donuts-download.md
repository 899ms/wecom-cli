---
'@wecom/cli': minor
'@wecom/cli-darwin-arm64': minor
'@wecom/cli-darwin-x64': minor
'@wecom/cli-linux-arm64': minor
'@wecom/cli-linux-x64': minor
'@wecom/cli-win32-x64': minor
---

重构下载落盘位置与 `--output-dir` 语义
  - 下载产生的文件（运行时二进制响应、响应内容导出为文件）默认落盘到当前工作目录，不再是 `<tmp_dir>/requests`——对齐 wget / curl `-O` / `gh release download` 的默认行为。
  - `--output-dir` 只对产生文件的响应生效；不再把 JSON 响应落盘为 `<method>.json`（分页时为 `<method>.ndjson`），纯 JSON 响应不产生文件。保存 JSON 响应请改用 `--output/-o` 或 shell 重定向。
  - 显式声明的 `--output-dir` 一律经沙箱校验，目标必须落在当前工作目录或系统临时目录内。
  - 移除 `tmp_dir` 配置（`WECOM_CLI_TMP_DIR` 环境变量、`config.json` 的 `tmp_dir` 字段及对应 builder API）与派生的 `request_storage_dir`。嵌入方可经 `ClientBuilder::default_output_dir` 设置客户端级默认下载目录（默认当前工作目录；单次调用仍可用 `--output-dir` 覆盖）。
