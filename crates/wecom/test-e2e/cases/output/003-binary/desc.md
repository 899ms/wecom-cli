# 二进制响应下载

- **场景**：验证 `application/octet-stream` 响应被正确保存为文件
- **Transport**：HTTP（wiremock）

## 测试等级

**P1**（application/octet-stream 响应正确保存为文件）
- **条件**：mock 返回 Content-Type: application/octet-stream
- **断言**：二进制数据正确写入输出文件，内容完整

## 前置条件

- wiremock 挂载 discovery + method call mock 返回二进制 + `Content-Disposition: attachment; filename="report.xlsx"`

## 命令

```rust
client.run(hr_dept_list_argv(&["--output-dir", "/tmp"]))
```

## 断言 — CLI

- `run` 返回 `Ok`
- stdout 为 `DownloadResult` JSON，`content_type` = `"application/octet-stream"`
- `file_path` 包含 `report.xlsx`
- `size` 与二进制内容长度一致
