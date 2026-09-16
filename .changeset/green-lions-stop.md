---
'@wecom/cli': minor
'@wecom/cli-darwin-arm64': minor
'@wecom/cli-darwin-x64': minor
'@wecom/cli-linux-arm64': minor
'@wecom/cli-linux-x64': minor
'@wecom/cli-win32-x64': minor
---

移除文件路径模糊纠错
  - 路径不存在或输入有误时，不再自动纠正为名称相近的真实文件；统一以「找不到目标文件」报错，不会按纠正后的路径静默继续。
