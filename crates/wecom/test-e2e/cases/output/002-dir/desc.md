# `--output-dir` 目录输出

- **场景**：验证 `--output-dir` flag 自动生成文件名
- **Transport**：HTTP（wiremock）

## 测试等级

**P0**（--output-dir flag 自动生成文件名并保存结果）
- **条件**：mock 返回 JSON 响应，命令行传入 --output-dir
- **断言**：dir 下生成自动命名文件，内容与响应一致

## 前置条件

- wiremock 挂载 discovery + method call mock

## 命令

```rust
client.run(hr_dept_list_argv(&["--output-dir", "/tmp"]))
```

## 断言 — CLI

- `run` 返回 `Ok`
- stdout 为 `DownloadResult` JSON，`file_path` 包含 `hr_department_list`
