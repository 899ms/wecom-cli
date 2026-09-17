---
'@wecom/cli': patch
'@wecom/cli-darwin-arm64': patch
'@wecom/cli-darwin-x64': patch
'@wecom/cli-linux-arm64': patch
'@wecom/cli-linux-x64': patch
'@wecom/cli-win32-x64': patch
---

支持 WECOM_CLI_ACCESS_TOKEN 环境变量直接指定 Bearer token
  - 设置该环境变量即直接提供 access token，优先于 `credentials.enc` 中保存的 token；空值视为未设置，回退文件来源。
  - 该来源不配套 bot 凭据：token 失效（后台 errcode 853004）时不发起静默刷新，返回鉴权错误提示更新该环境变量后重试。
  - 取消 `custom-access-token` 编译开关，所有构建均生效。
