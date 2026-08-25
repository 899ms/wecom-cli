---
"@wecom/cli": patch
---

multipart 上传请求支持 token 失效重放

- 请求载荷统一为可重放的延迟工厂：multipart 表单在每次发送时重新构建（重新打开文件），token 失效（853004）静默刷新后的自动重放不再限于 JSON 请求，文件上传类调用同样生效。
