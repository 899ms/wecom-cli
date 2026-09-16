---
'@wecom/cli': patch
'@wecom/cli-darwin-arm64': patch
'@wecom/cli-darwin-x64': patch
'@wecom/cli-linux-arm64': patch
'@wecom/cli-linux-x64': patch
'@wecom/cli-win32-x64': patch
---

外部路径相对值统一锚定到进程工作目录
  - `WECOM_CLI_CONFIG_DIR` 的相对值按进程工作目录解析为绝对路径；空值按未设置处理，回退默认配置目录。
