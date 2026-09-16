---
'@wecom/cli': minor
'@wecom/cli-darwin-arm64': minor
'@wecom/cli-darwin-x64': minor
'@wecom/cli-linux-arm64': minor
'@wecom/cli-linux-x64': minor
'@wecom/cli-win32-x64': minor
---

精简 JSON 自动修复提示，避免载荷外泄
  - `--json` / `--set` 输入被自动修复时，stderr 提示精简为单行，不再回显修复前后的 JSON 原文。
  - 遥测事件不再携带修复前后的载荷内容，仅保留修复策略信息，避免敏感数据外泄。
