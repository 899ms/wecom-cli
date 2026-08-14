---
"@wecom/cli": major
---

重构命令模型，接口集合整体更名

- 调用格式由 `wecom-cli <category> <method> [json_args]` 改为 `wecom-cli <service> [resource...] <method> [flags]`，方法支持嵌套资源路径。
- **接口名称变化**：旧版扁平工具名（内部经 MCP 工具集下发，如 `contact get_userlist`、`doc create_doc`、`msg get_msg_media`）整体替换为服务端 schema 下发的服务/资源/方法路径（如 `contact users search`、`doc contents get`、`message aibot sessions list`）；方法名与参数名均可能不同，现有脚本与集成必须按 `wecom-cli <service> <method> --help` / `--doc` 逐个核对迁移。
- 品类标识调整：`msg` 更名为 `message`，`schedule` 更名为 `calendar`；可用服务列表以 `wecom-cli --help` 实际输出为准。
- 请求体不再使用位置参数 JSON 字符串，改为三种可组合的方式：schema 生成的命名参数（如 `--id root`）、`--json '<JSON>'`、`--set path=value`。
- 服务目录与方法 schema 在线获取并本地缓存（TTL 60 秒），查看帮助与调用工具均需要凭证与网络。
- 媒体文件不再统一下载到临时目录，下载/落盘行为由 `--output` / `--output-dir` 控制，文件读写受沙箱目录约束。

迁移示例：

```bash
# 旧
wecom-cli contact get_userlist '{}'
wecom-cli doc create_doc '{"doc_type": 3, "doc_name": "项目周报"}'
# 新（方法名与参数以 --help 为准）
wecom-cli contact users search --json '{}'
wecom-cli doc create --json '{"doc_type": 3, "doc_name": "项目周报"}'
```
