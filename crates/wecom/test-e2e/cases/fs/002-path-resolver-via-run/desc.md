# `CliRun::resolver()` per-run 覆盖路径解析器

- **场景**：验证 Client 无全局 resolver 时，CliRun::resolver() 对单次 run 注入覆盖
- **Transport**：HTTP（wiremock）

## 前置条件

- wiremock 挂载 discovery + method call mock
- 临时目录 `physical_dir` 作为 writable 根
- Client 正常构建（无全局 resolver）
- 通过 `client.run(argv).resolver(custom_resolver)` 注入 per-run resolver

## 命令

```rust
client.run(hr_dept_list_argv(&["--output", "virtual://run_out.json"]))
    .resolver(resolver)
```

## 断言 — CLI

- `run` 返回 `Ok`
- stdout 为 `DownloadResult` JSON

## 断言 — HTTP Endpoint

- `POST /department/list` 被调用 1 次

## 断言 — FS

- `<physical_dir>/run_out.json` 被创建
- 文件内容为 API 响应的 JSON

## 关键上下文

- `crates/wecom/src/client/run.rs`：`CliRun::resolver()` → `self.fs.resolver_mut()` 直接覆盖 `Fs` 的 resolver。
- `crates/wecom/src/fs/mod.rs`：`Fs::resolve()` 使用 `self.resolver` 做路径映射。
