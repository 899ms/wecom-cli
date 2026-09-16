---
'@wecom/cli': minor
'@wecom/cli-darwin-arm64': minor
'@wecom/cli-darwin-x64': minor
'@wecom/cli-linux-arm64': minor
'@wecom/cli-linux-x64': minor
'@wecom/cli-win32-x64': minor
---

收紧文件读写沙箱
  - CLI 此前不注入任何沙箱根、可读写任意位置；现在读写均限制在当前工作目录与系统临时目录内。凭据目录与配置目录（含任意基座下的 `**/.config/wecom`）一律拒绝。
  - 内置 deny 清单覆盖凭据形状路径（`.ssh`、`.aws`、`.kube`、`.env`、`.git`、`*.pem`、`*.key`、`id_rsa` 等）与系统目录，即使位于当前工作目录内同样拒绝。
  - 迁移指引：如需读写当前工作目录之外的文件，请在目标目录下运行 CLI，或使用相对当前工作目录的路径。`--output` / `--output-dir` 仍控制文件落点，但目标必须落在上述范围内。
  - 错误码与错误输出格式不变（CLI 段 893000–893299；后台 errcode 原样透传）。
