# 后台 10021 错误码展示当前命令 help

- **场景**：`wecom hr department list` 时 method 接口返回网关错误信封 `error.code = 10021`，`run` 返回 `Err`（`exit_code()` 为 1），且 stdout 额外输出 `error: <后台 errmsg>` 行 + 空行 + `department list` 子命令的 help（对齐 clap 错误输出格式）
- **Transport**：HTTP（wiremock）
- **对齐**：`CliRun::execute` 分发结果后，若错误为 `Error::Transport(Api { code: 10021 })`，则渲染当前叶子子命令 help 并打印到 output

## 前置条件

- 挂载标准 discovery mocks（catalog + hr 服务详情）
- 挂载 `/department/list` method mock，返回 `error.code = 10021`

## 断言 — CLI

- `wecom hr department list --id root` 返回 `Err`，`exit_code()` 为 1
- stdout 包含 `error: invalid usage`（后台 errmsg 行）、`Usage`（help 头部）与 `List departments`（method 描述）

## 关键上下文

- `client/run.rs`：`CliRun::execute` 在分发后直接匹配 `Error::Transport(Api { code: Some(10021) })`，命中则经 `render_leaf_help` 渲染当前子命令 help（`error:` 前缀并入默认文案）并打印，错误仍继续传播
