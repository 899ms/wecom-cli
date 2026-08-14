# 二进制响应默认下载到 `tmp_dir`

- **场景**：无 `--output` / `--output-dir` 时，非 JSON 响应下载到 `tmp_dir` 目录
- **Transport**：HTTP（wiremock）

## 测试等级

**P1**（无 --output/--output-dir 时二进制响应默认下载到 tmp_dir）
- **条件**：mock 返回非 JSON 响应，不传输出相关 flag
- **断言**：文件下载到 tmp_dir，内容正确

## 前置条件

- wiremock 挂载 discovery + method call mock 返回 `application/octet-stream`

## 命令

```rust
client.run(hr_dept_list_argv(&[]))
```

## 断言

- `run` 返回 `Ok`
- stdout 为 `DownloadResult` JSON，`file_path` 在自定义 `tmp_dir` 下
