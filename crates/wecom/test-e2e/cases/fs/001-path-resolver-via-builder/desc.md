# `ClientBuilder::path_resolver()` 注入自定义路径解析器

- **场景**：验证 Builder 注入的 PathResolver 在 run 期间将虚拟路径映射到物理路径
- **Transport**：HTTP（wiremock）

## 前置条件

- wiremock 挂载 discovery + method call mock
- 临时目录 `physical_dir` 作为 writable 根
- Client 通过 `ClientBuilder::path_resolver()` 注入 resolver，将 `virtual://` 前缀映射到 `physical_dir`

## 命令

```rust
client.run(hr_dept_list_argv(&["--output", "virtual://out.json"]))
```

## 断言 — CLI

- `run` 返回 `Ok`
- stdout 为 `DownloadResult` JSON，`file_path` 为解析后的物理路径（非 `virtual://` 前缀）

## 断言 — HTTP Endpoint

- `POST /department/list` 被调用 1 次

## 断言 — FS

- `<physical_dir>/out.json` 被创建
- 文件内容为 API 响应的 JSON

## 关键上下文

- `crates/wecom/src/fs/mod.rs`：`Fs::resolve()` → resolver 在 normalize 前将 `virtual://` 映射到物理路径。
- `crates/wecom/src/client/builder.rs`：`path_resolver()` setter 存入 `ClientBuilder.path_resolver`，`build()` 中传入 `Client.path_resolver`。
- `crates/wecom/src/client/mod.rs`：`default_fs()` 将 resolver 注入 `Fs`。
