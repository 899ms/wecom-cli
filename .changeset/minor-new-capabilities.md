---
"@wecom/cli": minor
---

新增服务品类与执行能力

- 服务品类扩展：新增 `mail`（邮件）、`sheet`（在线表格）、`smartpage`（智能文档）、`disk`（微盘）、`media`（媒体文件）、`identity`（身份）等，文档能力增强（搜索、重命名、权限管理）。
- 本地 helper：以 `+` 前缀挂载在命令路径上（如媒体上传、文件导入场景）。
- 执行 flag：`--dry-run` 本地校验并打印请求；`--page-count` / `--page-delay` 游标自动分页（NDJSON 输出）；`--output` / `-o` 与 `--output-dir` 将响应与附件落盘（0600）。
- 文档与调试：`--doc` / `--schema` 输出服务或方法文档与 schema；内建 `schema list|get`、`cache status|clear` 命令。
- 长任务自动轮询：后台返回 `taskid` 时按配置轮询直至完成或超时。
- `--json` / `--set` 中的非法 JSON 片段自动修复，并在 stderr 输出修复前后对照。
