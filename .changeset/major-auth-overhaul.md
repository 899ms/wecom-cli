---
"@wecom/cli": major
---

重构鉴权命令与凭据存储

- `wecom-cli init` 移除，改为 `wecom-cli auth init`（交互式选择接入方式，非交互环境自动扫码），并新增 `wecom-cli auth show` 查看授权状态。
- 扫码接入成为默认方式（终端二维码，5 分钟超时），支持 `--noninteractive` / `--no-browser` / `--output-qrcode` / `--manual`。
- `auth show` 输出调整：默认输出人类可读的 `Status` 与 `Bot ID`；原 `--auth-status` 改为 `--status`，仅输出 `authorized` / `unauthorized` 单行。
- 凭据文件由 `<config_dir>/bot.enc` 改为 `<config_dir>/credentials.enc`（AES-256-GCM，0600，bot 信息与 token 共存）；启动时自动迁移旧版 `bot.enc`，旧文件保留。
- 加密密钥优先使用系统 keyring，不可用时回退 `<config_dir>/.encryption_key` 文件。
- token 失效（后台 errcode 853004）时使用 bot 凭据静默换取新 token 并重放一次请求，无需重新 `auth init`。

迁移指引：将脚本中的 `wecom-cli init` 替换为 `wecom-cli auth init`；`auth show --auth-status` 替换为 `auth show --status`。
