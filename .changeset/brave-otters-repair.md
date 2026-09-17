---
'@wecom/cli': patch
'@wecom/cli-darwin-arm64': patch
'@wecom/cli-darwin-x64': patch
'@wecom/cli-linux-arm64': patch
'@wecom/cli-linux-x64': patch
'@wecom/cli-win32-x64': patch
---

修正两处面向用户可感知的行为
  - env 来源 token 过期（后台 errcode 853004）时返回鉴权错误，指明 `WECOM_CLI_ACCESS_TOKEN` 已过期、需更新该环境变量后重试（此前该提示因刷新裁决前置门禁而不可达，用户只能看到原始业务错误）。
  - 修正编译期端点注入的构建缓存问题：`build.rs` 补登记 `WECOM_CLI_BASE_URL` / `WECOM_CLI_AUTH_ENDPOINT` 的 `rerun-if-env-changed`，构建时改动这两个变量现在会触发重新编译，不再复用缓存产物、把旧端点烘焙进二进制。
