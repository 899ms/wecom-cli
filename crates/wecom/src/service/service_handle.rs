use std::sync::Arc;

use super::{MethodHandle, MethodSummary, ServiceSchemaInfo, alias, doc};
use crate::client::Client;
use crate::helpers::Helper;
use crate::registry::{ServiceResource, ServiceSchema};
use crate::telemetry::contract::method_alias;
use crate::{Error, Result, telemetry};

/// Handle representing a discovered service.
///
/// Created via [`Client::service()`] which fetches the service description.
/// All accessor methods are synchronous since the data is already loaded.
pub struct ServiceHandle<'c> {
    pub(crate) client: &'c Client,
    pub(crate) name: String,
    pub(crate) schema: Arc<ServiceSchema>,
}

impl<'c> ServiceHandle<'c> {
    pub(crate) fn new(client: &'c Client, name: String, schema: Arc<ServiceSchema>) -> Self {
        Self {
            client,
            name,
            schema,
        }
    }

    /// Service name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Service description text.
    pub fn description(&self) -> Option<&str> {
        self.schema.description.as_deref()
    }

    /// Skills provided by the backend for this service,
    /// used for user guidance when a method is not found.
    pub fn skills(&self) -> &[String] {
        &self.schema.skills
    }

    /// List all available methods (recursively flattened from the resource tree).
    pub fn methods(&self) -> Vec<MethodHandle<'c>> {
        let mut result = Vec::new();
        collect_method_handles(
            self.client,
            &self.schema,
            &self.schema.resource_tree,
            &[self.name.as_str()],
            &mut result,
        );
        result
    }

    /// Get a handle to a specific method by path.
    ///
    /// Before walking the resource tree, attempts path-alias resolution
    /// (see [`crate::registry::MethodSchema::path_alias`]), so callers
    /// don't need to distinguish alias paths from real command paths.
    ///
    /// # Example
    /// ```ignore
    /// let method = svc.method(&["users", "list"])?;
    /// ```
    pub fn method(&self, path: &[&str]) -> Result<MethodHandle<'c>> {
        if path.is_empty() {
            return Err(Error::Validation("方法路径不能为空".to_string()))
                .inspect_err(|e| tracing::error!(error = %e, "empty method path"));
        }

        let input_path: Vec<_> = std::iter::once(self.name.clone())
            .chain(path.iter().map(|s| s.to_string()))
            .collect();

        // alloc 仅发生在 alias 命中时，未命中走原始 `path` 零 alloc）。
        let alias_entries = alias::collect_alias_entries(&self.name, &self.schema.resource_tree);
        let resolved = alias::resolve_command_path(&alias_entries, path);

        if let Some(resolved) = &resolved {
            telemetry::emit(
                method_alias::KIND,
                &serde_json::json!({
                    method_alias::FIELD_INPUT: input_path.join(" "),
                    method_alias::FIELD_RESOLVED: std::iter::once(self.name.as_str())
                        .chain(resolved.iter().map(|s| s.as_str()))
                        .collect::<Vec<_>>()
                        .join(" "),
                }),
            );
        }

        let resolved: Option<Vec<_>> =
            resolved.map(|real| real.iter().map(String::as_str).collect());
        let path = resolved.as_deref().unwrap_or(path);

        let mut resource = &self.schema.resource_tree;
        for &segment in &path[..path.len() - 1] {
            resource = resource.resources.get(segment).ok_or_else(|| {
                Error::Other(format!("找不到目标方法 '{}'", path.join(".")).into())
            }).inspect_err(|_| tracing::error!(error = %format!("找不到目标方法 '{}'", path.join(".")), "method not found"))?;
        }

        let method_name = path[path.len() - 1];
        let method = resource.methods.get(method_name).ok_or_else(|| {
            Error::Other(format!("找不到目标方法 '{}'", path.join(".")).into())
        }).inspect_err(|_| tracing::error!(error = %format!("找不到目标方法 '{}'", path.join(".")), "method not found"))?;

        let method_path_segments: Vec<_> = std::iter::once(self.name.clone())
            .chain(path.iter().copied().map(String::from))
            .collect();

        Ok(MethodHandle {
            client: self.client,
            service_schema: Arc::clone(&self.schema),
            method_name: method_name.to_string(),
            method_path_segments,
            schema: method.clone(),
        })
    }

    /// Look up a helper registered under this service by sub-path.
    ///
    /// The `path` is the command path relative to the service **including the
    /// helper's leaf command name**, e.g. `&["+download"]` resolves to the
    /// helper located at group `["<service>"]` with name `"+download"`.
    pub fn helper(&self, path: &[&str]) -> Option<&dyn Helper> {
        let full_path: Vec<_> = std::iter::once(self.name.as_str())
            .chain(path.iter().copied())
            .collect();
        self.client.helper_registry().get_helper(&full_path)
    }

    /// Generate service-level documentation (Markdown).
    ///
    /// Returns a [`StyledStr`](clap::builder::StyledStr) that embeds ANSI
    /// styling.  Use `.ansi()` to render with colour or `Display` to
    /// strip escape codes.
    pub fn doc(&self) -> clap::builder::StyledStr {
        doc::gen_service_doc(self.client.bin_name(), &self.name, &self.schema, 1)
    }

    /// Generate service-level schema (JSON).
    pub fn schema(&self) -> ServiceSchemaInfo {
        let methods = self
            .methods()
            .into_iter()
            .map(|h| MethodSummary {
                name: h.path().join("."),
                description: h.description().map(str::to_string),
            })
            .collect();

        ServiceSchemaInfo {
            name: self.name.clone(),
            description: self.schema.description.clone(),
            skills: self.schema.skills.clone(),
            methods,
        }
    }
}

/// Recursively collect all methods from a resource tree as `MethodHandle`s.
fn collect_method_handles<'c>(
    client: &'c Client,
    schema: &Arc<ServiceSchema>,
    resource: &ServiceResource,
    prefix: &[&str],
    result: &mut Vec<MethodHandle<'c>>,
) {
    for (name, method) in &resource.methods {
        let mut full_path: Vec<_> = prefix.iter().map(|s| s.to_string()).collect();
        full_path.push(name.clone());
        result.push(MethodHandle {
            client,
            service_schema: Arc::clone(schema),
            method_name: name.clone(),
            method_path_segments: full_path,
            schema: method.clone(),
        });
    }
    for (name, child_resource) in &resource.resources {
        let mut child_prefix = prefix.to_vec();
        child_prefix.push(name.as_str());
        collect_method_handles(client, schema, child_resource, &child_prefix, result);
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：ServiceHandle（服务句柄）
    //!
    //! ### 关键接口
    //! - [ServiceHandle::new] — 构造服务句柄
    //! - [ServiceHandle::name] — 返回服务名
    //! - [ServiceHandle::description] — 返回服务描述
    //! - [ServiceHandle::method] — 按路径查找方法
    //! - [ServiceHandle::methods] — 返回所有方法的扁平列表
    //! - [ServiceHandle::helper] — 按路径查找 helper
    //!
    //! ### 关键分支与异常路径
    //! - method 空路径 → Err
    //! - method 不存在的资源 → Err
    //! - method 资源存在但方法不存在 → Err
    //! - helper 不存在的路径 → None
    //! - description 未设置 → None
    //!
    //! ### 上下游交互
    //! - 上游：[ServiceRegistry] 创建 ServiceHandle 供调用方使用
    //! - 下游：依赖 [Client] 执行 HTTP 请求

    use super::*;
    use crate::registry;

    /// Build an isolated [Client] for unit tests.
    fn build_isolated_client() -> Client {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Client::builder().home_dir(&dir).cwd(&dir).build().unwrap()
    }

    static TEST_CLIENT: std::sync::LazyLock<Client> =
        std::sync::LazyLock::new(build_isolated_client);

    fn svc() -> ServiceHandle<'static> {
        let s = Arc::new(registry::ServiceSchema {
            description: Some("TS".into()),
            skills: vec![],
            base_url: Some("https://a.test/".to_string()),
            schemas: indexmap::IndexMap::new(),
            resource_tree: registry::ServiceResource {
                methods: {
                    let mut m = indexmap::IndexMap::new();
                    m.insert(
                        "list".into(),
                        registry::MethodSchema {
                            description: Some("L".into()),
                            http_method: "GET".into(),
                            path: "/list".into(),
                            path_alias: None,
                            request: None,
                            response: None,
                            ..Default::default()
                        },
                    );
                    m
                },
                resources: {
                    let mut r = indexmap::IndexMap::new();
                    r.insert(
                        "users".into(),
                        registry::ServiceResource {
                            methods: {
                                let mut m = indexmap::IndexMap::new();
                                m.insert(
                                    "get".into(),
                                    registry::MethodSchema {
                                        description: None,
                                        http_method: "GET".into(),
                                        path: "/u/{i}".into(),
                                        path_alias: None,
                                        request: None,
                                        response: None,
                                        ..Default::default()
                                    },
                                );
                                m
                            },
                            resources: indexmap::IndexMap::new(),
                            ..Default::default()
                        },
                    );
                    r
                },
                ..Default::default()
            },
        });
        ServiceHandle::new(&TEST_CLIENT, "svc".into(), s)
    }

    /// P0：[ServiceHandle::name] 返回正确的服务名
    /// 条件：创建名为 "svc" 的 ServiceHandle
    /// 断言：name() 返回 "svc"
    #[test]
    fn service_name_returns_correct_name() {
        assert_eq!(svc().name(), "svc");
    }

    /// P0：[ServiceHandle::description] 在设置描述时返回 Some
    /// 条件：服务 schema 的 description 为 "TS"
    /// 断言：description() 返回 Some("TS")
    #[test]
    fn service_description_returns_some_when_set() {
        assert_eq!(svc().description(), Some("TS"));
    }

    /// P1：[ServiceHandle::method] 在空路径时返回错误
    /// 条件：传入空路径 &[]
    /// 断言：返回 Err
    #[test]
    fn method_with_empty_path_returns_error() {
        assert!(svc().method(&[]).is_err());
    }

    /// P1：[ServiceHandle::method] 在不存在的资源路径时返回错误
    /// 条件：传入 ["x", "y"]（资源树中无 "x"）
    /// 断言：返回 Err
    #[test]
    fn method_with_bad_resource_returns_error() {
        assert!(svc().method(&["x", "y"]).is_err());
    }

    /// P1：[ServiceHandle::method] 在资源存在但方法不存在时返回错误
    /// 条件：传入 ["users", "del"]（users 资源下无 del 方法）
    /// 断言：返回 Err
    #[test]
    fn method_with_nonexistent_method_returns_error() {
        assert!(svc().method(&["users", "del"]).is_err());
    }

    /// P0：[ServiceHandle::method] 通过名称直接获取顶层方法
    /// 条件：传入 ["list"]（顶层有 list 方法）
    /// 断言：返回 Ok，name() 为 "list"
    #[test]
    fn method_returns_direct_method_by_name() {
        let m = svc().method(&["list"]).unwrap();
        assert_eq!(m.name(), "list");
    }

    /// P0：[ServiceHandle::method] 通过路径获取嵌套资源下的方法
    /// 条件：传入 ["users", "get"]（users 资源下有 get 方法）
    /// 断言：返回 Ok，name() 为 "get"
    #[test]
    fn method_returns_nested_resource_method() {
        let m = svc().method(&["users", "get"]).unwrap();
        assert_eq!(m.name(), "get");
    }

    /// P0：[ServiceHandle::methods] 返回资源树中所有方法的扁平列表
    /// 条件：服务有顶层 "list" 方法和嵌套 users 下的 "get" 方法
    /// 断言：结果中同时包含 name 为 "list" 和 "get" 的方法
    #[test]
    fn methods_returns_all_flattened_methods() {
        let b = svc().methods();
        assert!(b.iter().any(|m| m.name() == "list"));
        assert!(b.iter().any(|m| m.name() == "get"));
    }

    /// 构造带 alias 的 contact 服务句柄：
    /// users.search 的 path_alias 为 `/contact/search`。
    fn svc_with_alias() -> ServiceHandle<'static> {
        let mut user_methods = indexmap::IndexMap::new();
        user_methods.insert(
            "search".to_string(),
            registry::MethodSchema {
                description: None,
                http_method: "POST".into(),
                path: "/contact/users/search".into(),
                path_alias: Some(vec!["/contact/search".to_string()]),
                request: None,
                response: None,
                ..Default::default()
            },
        );
        let users = registry::ServiceResource {
            methods: user_methods,
            resources: indexmap::IndexMap::new(),
            ..Default::default()
        };
        let mut top = indexmap::IndexMap::new();
        top.insert("users".into(), users);
        let s = Arc::new(registry::ServiceSchema {
            description: None,
            skills: vec![],
            base_url: Some("https://a.test/".to_string()),
            schemas: indexmap::IndexMap::new(),
            resource_tree: registry::ServiceResource {
                methods: indexmap::IndexMap::new(),
                resources: top,
                ..Default::default()
            },
        });
        ServiceHandle::new(&TEST_CLIENT, "contact".into(), s)
    }

    /// P0：[ServiceHandle::method] alias 路径命中后改走真实路径，返回真实方法 handle
    /// 条件：users.search 声明 alias `/contact/search`；调用 `method(&["search"])`
    /// 断言：返回的 handle name 为 "search"，full path 为 ["contact","users","search"]
    #[test]
    fn method_resolves_alias_to_real_path() {
        let h = svc_with_alias().method(&["search"]).unwrap();
        assert_eq!(h.name(), "search");
        assert_eq!(
            h.path(),
            &[
                "contact".to_string(),
                "users".to_string(),
                "search".to_string()
            ]
        );
    }

    /// P0：[ServiceHandle::method] 真实路径与 alias 共存时仍能正常解析真实路径
    /// 条件：users.search 已声明 alias，但调用方仍传入真实路径 ["users","search"]
    /// 断言：依然返回 search 方法 handle（alias 未屏蔽真实路径）
    #[test]
    fn method_real_path_works_when_alias_exists() {
        let h = svc_with_alias().method(&["users", "search"]).unwrap();
        assert_eq!(h.name(), "search");
    }

    /// P1：[ServiceHandle::method] 既不命中 alias 也不命中真实路径时返回错误
    /// 条件：alias 表内没有 ["unknown"]，真实树里也没有
    /// 断言：返回 Err
    #[test]
    fn method_unknown_path_returns_error_with_alias_table() {
        assert!(svc_with_alias().method(&["unknown"]).is_err());
    }

    /// P1：[ServiceHandle::helper] 对不存在的 helper 路径返回 None
    /// 条件：传入 ["x", "y"]（无对应 helper）
    /// 断言：返回 None
    #[test]
    fn helper_returns_none_for_nonexistent_path() {
        assert!(svc().helper(&["x", "y"]).is_none());
    }

    /// P0：[ServiceHandle::description] 在未设置描述时返回 None
    /// 条件：服务 schema 的 description 为 None
    /// 断言：description() 返回 None
    #[test]
    fn service_description_returns_none_when_unset() {
        let s = Arc::new(registry::ServiceSchema {
            description: None,
            skills: vec![],
            base_url: Some("https://a.test/".to_string()),
            schemas: indexmap::IndexMap::new(),
            resource_tree: registry::ServiceResource {
                methods: indexmap::IndexMap::new(),
                resources: indexmap::IndexMap::new(),
                ..Default::default()
            },
        });
        let handle = ServiceHandle::new(&TEST_CLIENT, "empty".into(), s);
        assert!(handle.description().is_none());
    }
}
