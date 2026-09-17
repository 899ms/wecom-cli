---
'@wecom/cli': patch
'@wecom/cli-darwin-arm64': patch
'@wecom/cli-darwin-x64': patch
'@wecom/cli-linux-arm64': patch
'@wecom/cli-linux-x64': patch
'@wecom/cli-win32-x64': patch
---

支持在构建时通过 WECOM_CLI_BASE_URL 指定默认请求端点
  - 构建时设置该环境变量即烘焙为默认 base URL，无需启用 `custom-endpoint` feature。
  - 优先级：运行时来源（`custom-endpoint` 下的 `WECOM_CLI_BASE_URL` 环境变量 / `config.json`）> 构建时值 > 内置默认端点。
