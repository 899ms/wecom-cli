---
"@wecom/cli": minor
---

支持远程文档渲染

- schema 中 service / resource / method 任一层级声明 `remote_doc: true` 时，对应节点的 `--doc` / `--help` / `--schema` 不再本地渲染，改为请求远程 endpoint 生成文档并直接输出。
- 生效规则为就近覆盖（method → 父级 resource 链 → service → 默认 `false`），每层可用 `remote_doc: false` 显式关闭上层开启的远程渲染。
