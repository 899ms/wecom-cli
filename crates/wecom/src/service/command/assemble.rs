use std::collections::HashSet;
use std::str::FromStr;

use clap::ArgMatches;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Number, Value};

use super::arg_types::{HelperCmdArgs, MethodCmdArgs};
use super::schema_clap::matches_to_value;
use crate::schema::{AdditionalProperties, JsonSchema};
use crate::telemetry::contract as ctr;
use crate::{Error, Result, json_path, telemetry};

// ── RequestArgs trait ─────────────────────────────────────

/// 统一 helper / method 参数结构的 trait，抹平差异供 [`assemble_payload`] 使用。
///
/// 要求实现 `Serialize + DeserializeOwned`，以便 [`apply_extras`] 通过 serde
/// 自省提取 `--json` 体中嵌入的 CLI flag。
pub(crate) trait RequestArgs: Serialize + DeserializeOwned {
    fn json(&self) -> Option<&str>;
    fn set_ops(&self) -> &[String];
}

impl RequestArgs for HelperCmdArgs {
    fn json(&self) -> Option<&str> {
        self.json.as_deref()
    }

    fn set_ops(&self) -> &[String] {
        &self.set
    }
}

impl RequestArgs for MethodCmdArgs {
    fn json(&self) -> Option<&str> {
        self.json.as_deref()
    }

    fn set_ops(&self) -> &[String] {
        &self.set
    }
}

// ── 统一请求体装配 ────────────────────────────────────────

/// 一次性装配请求体：parse `--json` → 抽 extras（mutates args）→ 合并 schema flag →
/// 应用 `--set`（最高优先级）。内部不短路；doc/schema/help 由调用方在其返回后、
/// 读已填好的 args 决定。
pub(crate) fn assemble_payload<A: RequestArgs>(
    args: &mut A,
    schema: Option<&JsonSchema>,
    matches: &ArgMatches,
) -> Result<Value> {
    // 1. --json 骨架（lenient，带 jsonrepair 容错）
    let mut payload = match args.json() {
        Some(j) => {
            repair_json(j).map_err(|e| Error::Validation(format!("--json 请求体解析失败: {e}")))?
        }
        None => serde_json::json!({}),
    };
    if let (Some(s), Some(obj)) = (schema, payload.as_object_mut()) {
        // 2. 抽取 flag extras（doc/schema/help/dry_run/page_count 等可来自 JSON 体）
        apply_extras(args, obj, s);
        // 3. 顶层 schema flag 覆盖同名 key
        obj.extend(matches_to_value(s, matches)?);
    }
    // 4. --set 深层覆盖（最高优先级）
    apply_set_ops(&mut payload, args.set_ops(), schema)?;
    Ok(payload)
}

// ── JSON extra extraction ─────────────────────────────────

/// Core logic for extracting CLI flag values embedded in the `--json` payload.
///
/// 1. **Applied & removed** — a non-schema key whose *normalized* form
///    (`--page-count` → `page_count`) matches an unset CLI field and whose
///    value deserializes into that field.
/// 2. **Kept untouched** — everything else: schema-defined keys, keys that
///    fail to parse, and keys that conflict with an already-set CLI field.
///    These retain their **original** key spelling and value, honouring the
///    "leave it as-is on failure" contract. Normalization is only ever used
///    as a lookup probe; it never rewrites the stored key.
fn apply_extras<T: DeserializeOwned + Serialize>(
    args: &mut T,
    payload: &mut serde_json::Map<String, Value>,
    schema: &JsonSchema,
) {
    // Collect ALL schema-defined property keys so hidden properties like
    // `output_dir` (x-wecom-hidden) stay in the payload instead of being
    // mistakenly treated as extra CLI args and silently dropped.
    let schema_keys: HashSet<_> = schema.properties.keys().map(|k| k.as_str()).collect();

    // Serialize `args` once to introspect its fields — which keys it
    // recognizes and whether they already have a non-default value.
    // `#[serde(skip)]` fields (e.g. `json`) are excluded on purpose;
    // the raw `--json` body is already consumed before this runs.
    let Ok(Value::Object(mut args_state)) = serde_json::to_value(&*args) else {
        return;
    };

    let mut args_extras = serde_json::Map::new();
    let mut payload_kept = serde_json::Map::new();
    for (payload_key, payload_value) in std::mem::take(payload) {
        if schema_keys.contains(payload_key.as_str()) {
            payload_kept.insert(payload_key, payload_value);
            continue;
        }

        let field = normalize_extra_key(&payload_key);

        // Bool(false) is treated as unset because clap fills Option<bool> +
        // SetTrue fields with Some(false) by default.
        let arg_is_unset = args_state
            .get(&field)
            .is_some_and(|v| v.is_null() || v == &Value::Bool(false));

        if arg_is_unset
            && serde_json::from_value::<T>(serde_json::json!({ &field: payload_value.clone() }))
                .is_ok()
        {
            args_extras.insert(field, payload_value);
        } else {
            payload_kept.insert(payload_key, payload_value);
        }
    }

    if !args_extras.is_empty() {
        args_state.extend(args_extras);
        if let Ok(merged) = serde_json::from_value::<T>(Value::Object(args_state)) {
            *args = merged;
        }
    }

    *payload = payload_kept;
}

/// Normalize an extra JSON key so serde can match it to struct fields.
///
/// Handles `--page-count`, `--page_count`, `page-count`, `page_count`
/// — all result in `page_count`.
fn normalize_extra_key(key: &str) -> String {
    let stripped = key.strip_prefix("--").unwrap_or(key);
    stripped.replace('-', "_")
}

// ── JSON 解析 ─────────────────────────────────────────────

/// Parse JSON with a jsonrepair fallback, returning the **raw** repair reason
/// (a plain `String`) on failure — not an [`Error`].
///
/// Emits `json_repair` telemetry on the repair path. Callers attach their own
/// user-facing context (`--json` body vs `--set` value) around the returned
/// `String`. Keeping the error un-wrapped here is what lets the sole
/// `Error::Validation` wrap happen once at the top, avoiding double-wrapping
/// like `Validation("... {Validation}")`.
fn repair_json(json: &str) -> std::result::Result<Value, String> {
    // Fast path: valid JSON.
    if let Ok(v) = serde_json::from_str(json) {
        return Ok(v);
    }
    tracing::info!("standard JSON parse failed, attempting repair");
    match jsonrepair_rs::jsonrepair_value(json) {
        Ok(v) => {
            tracing::info!("JSON repaired successfully");
            let repaired = serde_json::to_string(&v).unwrap_or_default();
            telemetry::emit(
                ctr::json_repair::KIND,
                &serde_json::json!({
                    ctr::json_repair::FIELD_OUTCOME: ctr::json_repair::OUTCOME_OK_REPAIRED,
                    ctr::json_repair::FIELD_INPUT: json,
                    ctr::json_repair::FIELD_OUTPUT: repaired,
                }),
            );
            Ok(v)
        }
        Err(repair_err) => {
            telemetry::emit(
                ctr::json_repair::KIND,
                &serde_json::json!({
                    ctr::json_repair::FIELD_OUTCOME: ctr::json_repair::OUTCOME_ERR_REPAIR,
                    ctr::json_repair::FIELD_INPUT: json,
                }),
            );
            Err(repair_err.to_string())
        }
    }
}

// ── --set 编排 ────────────────────────────────────────────

/// `--set` 编排：逐项 split `=` → parse_path → resolve_set_value → upsert_value_deep。
fn apply_set_ops(
    payload: &mut Value,
    set_ops: &[String],
    schema: Option<&JsonSchema>,
) -> Result<()> {
    let mut typed_by_schema = 0u32;
    let count = set_ops.len() as u32;

    let result = (|| -> Result<()> {
        for raw in set_ops {
            let (path_str, rhs) = raw
                .split_once('=')
                .ok_or_else(|| Error::Validation(format!("--set {raw} 解析失败: 缺少 `=`")))?;
            let path = json_path::parse_path(path_str)
                .map_err(|e| Error::Validation(format!("--set {path_str} 路径解析失败: {e}")))?;
            let leaf_type = schema.and_then(|s| resolve_leaf_type(s, &path));
            if leaf_type.is_some() {
                typed_by_schema += 1;
            }
            // resolve_set_value / upsert_value_deep 均返回纯 String 原因，这里统一包一次
            // Error::Validation，避免出现 Validation("... {Validation}") 的重复包裹。
            let value = resolve_set_value(rhs, leaf_type.as_deref())
                .map_err(|e| Error::Validation(format!("--set {path_str} 值无效: {e}")))?;
            json_path::upsert_value_deep(payload, &path, value)
                .map_err(|e| Error::Validation(format!("--set {path_str} 赋值失败: {e}")))?;
        }
        Ok(())
    })();

    // Emit set_path telemetry only when --set is actually used
    if count > 0 {
        let outcome = match &result {
            Ok(_) => ctr::set_path::OUTCOME_OK,
            Err(_) => ctr::set_path::OUTCOME_ERR,
        };
        telemetry::emit(
            ctr::set_path::KIND,
            &serde_json::json!({
                ctr::set_path::FIELD_OUTCOME: outcome,
                ctr::set_path::FIELD_COUNT: count,
                ctr::set_path::FIELD_TYPED_BY_SCHEMA: typed_by_schema,
            }),
        );
    }

    result
}

/// 沿 path 在已展开的 schema 上逐段下潜，返回叶子 `type`。
///
/// `request_schema()` 返回的 schema 已递归展开所有 `$ref`（含 properties/items 深层），
/// 因此无需额外的 schemas 表即可直接下潜。任一级无法继续 → `None`（触发回退 B）。
fn resolve_leaf_type(top: &JsonSchema, path: &[json_path::PathSegment]) -> Option<String> {
    let mut current = top;

    for seg in path {
        match seg {
            json_path::PathSegment::Key(key) => {
                // 先查显式 properties；未命中则回退到 additionalProperties 的 schema（任意键）。
                current = if let Some(schema) = current.properties.get(key.as_str()) {
                    schema
                } else if let Some(AdditionalProperties::Schema(schema)) =
                    current.additional_properties.as_deref()
                {
                    schema
                } else {
                    return None;
                };
            }
            json_path::PathSegment::Index(_idx) => {
                current = current.items.as_ref()?;
            }
        }
    }

    current.schema_type.clone()
}

/// 依据 leaf_type（A）或推断（B）把 RHS 转成 Value。
///
/// 失败时返回**纯 [`String`] 原因**（不含 [`Error`] 包装），由调用方
/// [`apply_set_ops`] 统一附加 `--set <path>` 上下文并包一次 `Error::Validation`。
fn resolve_set_value(rhs: &str, leaf_type: Option<&str>) -> std::result::Result<Value, String> {
    match leaf_type {
        // A: schema 感知
        Some("string") => Ok(Value::String(
            serde_json::from_str(rhs).unwrap_or_else(|_| rhs.to_string()),
        )),
        Some("integer" | "number") => Number::from_str(rhs)
            .map(Value::Number)
            .map_err(|e| format!("`{rhs}` 不是有效的数值: {e}")),
        Some("boolean") => match rhs {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("`{rhs}` 不是有效的布尔值，应为 true 或 false")),
        },
        Some("array" | "object") => {
            repair_json(rhs).map_err(|e| format!("`{rhs}` 不是合法的 JSON: {e}"))
        }
        Some(_unknown) => {
            // Unknown schema type → fall back to strategy B inference
            resolve_set_value(rhs, None)
        }
        // B: 类型推断
        None => {
            let first = rhs.chars().next();
            if first == Some('{') || first == Some('[') {
                return repair_json(rhs).map_err(|e| format!("`{rhs}` 不是合法的 JSON: {e}"));
            }
            match rhs {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => {
                    if let Ok(n) = Number::from_str(rhs) {
                        Ok(Value::Number(n))
                    } else {
                        Ok(Value::String(
                            serde_json::from_str(rhs).unwrap_or_else(|_| rhs.to_string()),
                        ))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：assemble（--set 深层参数赋值与请求体装配）
    //!
    //! ### 关键接口
    //! - [assemble_payload] — 统一装配请求体（--json → extract extras → matches → --set）
    //! - [apply_set_ops] — --set 编排：逐项 split `=` → parse_path → resolve_set_value → upsert
    //! - [resolve_leaf_type] — 沿 path 在 schema 上逐段下潜，返回叶子 type（策略 A）
    //! - [resolve_set_value] — 依据 leaf_type（A）或推断（B）把 RHS 转成 Value
    //! - [RequestArgs] trait — 统一 helper/method 参数结构差异
    //!
    //! ### 关键分支与异常路径
    //! - resolve_set_value：A 有 schema → 精确定型；B 无 schema → 推断（{[\ → JSON，true/false/null → 直接，
    //!   数字 → Number，其余 → String）
    //! - resolve_leaf_type：沿已展开 schema 逐段下潜；key 不在 properties 时回退到
    //!   additional_properties Schema 查找类型；任一级无法继续 → None（回退 B）
    //! - apply_set_ops：缺 `=` → Err；路径非法 → Err；值非法 → Err；赋值冲突 → Err
    //!
    //! ### 上下游交互
    //! - 上游：[super::super::handler::handle_service_cmd] 调用 assemble_payload
    //! - 下游：依赖 [json_path]、[JsonSchema]、[telemetry]

    use std::sync::{Arc, Mutex};

    use indexmap::IndexMap;
    use serde_json::json;
    use tracing_subscriber::prelude::*;

    use super::*;
    use crate::telemetry::{CaptureScope, ClientEvent, EventExt, TelemetryLayer};

    // ── resolve_set_value ──

    /// P0：[resolve_set_value] 策略 A：叶子 string，RHS 形如数字
    /// 条件：leaf_type 为 "string"，输入 "98"
    /// 断言：返回 Value::String("98")
    #[test]
    fn resolve_set_value_string_keeps_numeric_as_string() {
        let v = resolve_set_value("98", Some("string")).unwrap();
        assert_eq!(v, Value::String("98".into()));
    }

    /// P0：[resolve_set_value] 策略 A：叶子 integer
    /// 条件：leaf_type 为 "integer"，输入 "100"
    /// 断言：返回 Number(100)
    #[test]
    fn resolve_set_value_integer_parsed() {
        let v = resolve_set_value("100", Some("integer")).unwrap();
        assert_eq!(v, serde_json::json!(100));
    }

    /// P1：[resolve_set_value] 策略 A：integer 但输入非法
    /// 条件：leaf_type 为 "integer"，输入 "abc"
    /// 断言：返回 Err
    #[test]
    fn resolve_set_value_integer_invalid_err() {
        assert!(resolve_set_value("abc", Some("integer")).is_err());
    }

    /// P1：[resolve_set_value] 策略 A：叶子 boolean
    /// 条件：leaf_type 为 "boolean"，输入 "true"
    /// 断言：返回 Bool(true)
    #[test]
    fn resolve_set_value_boolean() {
        let v = resolve_set_value("true", Some("boolean")).unwrap();
        assert_eq!(v, Value::Bool(true));
    }

    /// P1：[resolve_set_value] 策略 A：叶子 boolean 非法值
    /// 条件：leaf_type 为 "boolean"，输入 "yes"
    /// 断言：返回 Err
    #[test]
    fn resolve_set_value_boolean_invalid_err() {
        assert!(resolve_set_value("yes", Some("boolean")).is_err());
    }

    /// P0：[resolve_set_value] 策略 A：叶子 object，RHS 为 JSON 片段
    /// 条件：leaf_type 为 "object"，输入 r#"{"a":1}"#
    /// 断言：返回 {"a":1}
    #[test]
    fn resolve_set_value_object_json() {
        let v = resolve_set_value(r#"{"a":1}"#, Some("object")).unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    /// P0：[resolve_set_value] 策略 B：无 schema，纯数字
    /// 条件：leaf_type 为 None，输入 "42"
    /// 断言：返回 Number(42)
    #[test]
    fn resolve_set_value_infer_number() {
        let v = resolve_set_value("42", None).unwrap();
        assert_eq!(v, serde_json::json!(42));
    }

    /// P1：[resolve_set_value] 策略 B：无 schema，JSON 片段首字符 {
    /// 条件：leaf_type 为 None，输入 r#"{"a":1}"#
    /// 断言：返回 {"a":1}
    #[test]
    fn resolve_set_value_infer_json_object() {
        let v = resolve_set_value(r#"{"a":1}"#, None).unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    /// P1：[resolve_set_value] 策略 B：无 schema，含冒号时间串
    /// 条件：leaf_type 为 None，输入 "2026-07-03 23:59:59"
    /// 断言：返回 String("2026-07-03 23:59:59")（不误解析）
    #[test]
    fn resolve_set_value_infer_time_string() {
        let v = resolve_set_value("2026-07-03 23:59:59", None).unwrap();
        assert_eq!(v, Value::String("2026-07-03 23:59:59".into()));
    }

    /// P1：[resolve_set_value] 策略 B：JSON 数组片段
    /// 条件：leaf_type 为 None，输入 "[1,2,3]"
    /// 断言：返回 [1,2,3]
    #[test]
    fn resolve_set_value_infer_json_array() {
        let v = resolve_set_value("[1,2,3]", None).unwrap();
        assert_eq!(v, serde_json::json!([1, 2, 3]));
    }

    /// P0：[resolve_set_value] 策略 A：叶子 string，RHS 被 LLM 加引号 "\"hello\""
    /// 条件：leaf_type 为 "string"，输入 "\"hello\""
    /// 断言：返回 Value::String("hello")（引号被剥除，不进入请求体）
    #[test]
    fn resolve_set_value_string_strips_llm_quotes() {
        let v = resolve_set_value(r#""hello""#, Some("string")).unwrap();
        assert_eq!(v, Value::String("hello".into()));
    }

    /// P1：[resolve_set_value] 策略 B：无 schema，RHS 被 LLM 加引号 "\"hello\""
    /// 条件：leaf_type 为 None，输入 "\"hello\""
    /// 断言：返回 Value::String("hello")（引号被剥除）
    #[test]
    fn resolve_set_value_infer_strips_llm_quotes() {
        let v = resolve_set_value(r#""hello""#, None).unwrap();
        assert_eq!(v, Value::String("hello".into()));
    }

    /// P1：[resolve_set_value] 策略 B：无 schema，"\`"98\`"" 保持为字符串
    /// 条件：leaf_type 为 None，输入 "\"98\""
    /// 断言：返回 Value::String("98")（JSON 字符串字面量，内容为 "98"）
    #[test]
    fn resolve_set_value_infer_quoted_number_stays_string() {
        let v = resolve_set_value(r#""98""#, None).unwrap();
        assert_eq!(v, Value::String("98".into()));
    }

    /// P1：[resolve_set_value] 非法 JSON 字符串不误伤
    /// 条件：leaf_type 为 "string"，输入 "a\" and \"b"
    /// 断言：返回 Value::String("a\" and \"b")（parse 失败，原样保留）
    #[test]
    fn resolve_set_value_string_partial_quotes_unchanged() {
        let input = r#"a" and "b"#;
        let v = resolve_set_value(input, Some("string")).unwrap();
        assert_eq!(v, Value::String(input.into()));
    }

    // ── resolve_leaf_type ──

    /// P0：[resolve_leaf_type] 沿已展开 schema 下潜命中叶子 type
    /// 条件：构造已展开的嵌套 schema（模拟 request_schema() 的返回结果）
    /// 断言：resolve_leaf_type 返回正确 type
    #[test]
    fn resolve_leaf_type_follows_ref() {
        let mut schemas = IndexMap::new();
        schemas.insert(
            "Inner".to_string(),
            JsonSchema {
                schema_type: Some("object".to_string()),
                properties: {
                    let mut m = IndexMap::new();
                    m.insert(
                        "text".to_string(),
                        std::sync::Arc::new(JsonSchema {
                            schema_type: Some("string".to_string()),
                            ..Default::default()
                        }),
                    );
                    m
                },
                ..Default::default()
            },
        );
        schemas.insert(
            "Top".to_string(),
            JsonSchema {
                schema_type: Some("object".to_string()),
                properties: {
                    let mut m = IndexMap::new();
                    m.insert(
                        "inner".to_string(),
                        std::sync::Arc::new(JsonSchema {
                            schema_ref: Some("Inner".to_string()),
                            ..Default::default()
                        }),
                    );
                    m
                },
                ..Default::default()
            },
        );
        // 模拟 request_schema()：已递归展开所有 $ref
        let resolved =
            crate::schema::resolve_schema(&schemas, "Top").expect("resolve_schema should succeed");
        let path = json_path::parse_path("inner.text").unwrap();
        let t = resolve_leaf_type(&resolved, &path);
        assert_eq!(t.as_deref(), Some("string"));
    }

    /// P1：[resolve_leaf_type] 某级 key 的 schema_type 为 None 时返回 None
    /// 条件：已展开 schema 中 x 属性的 type 为 None
    /// 断言：返回 None（触发回退 B）
    #[test]
    fn resolve_leaf_type_missing_type_returns_none() {
        let top = JsonSchema {
            schema_type: Some("object".to_string()),
            properties: {
                let mut m = IndexMap::new();
                m.insert(
                    "x".to_string(),
                    std::sync::Arc::new(JsonSchema {
                        // 无 schema_type → resolve_leaf_type 返回 None
                        ..Default::default()
                    }),
                );
                m
            },
            ..Default::default()
        };
        let path = json_path::parse_path("x").unwrap();
        assert!(resolve_leaf_type(&top, &path).is_none());
    }

    /// P1：[resolve_leaf_type] key 不在 properties 中但匹配 additional_properties Schema 时解析成功
    /// 条件：schema 有 additional_properties=Schema("number")，查询任意 key
    /// 断言：返回 Some("number")
    #[test]
    fn resolve_leaf_type_falls_back_to_additional_properties_schema() {
        let top = JsonSchema {
            schema_type: Some("object".to_string()),
            properties: IndexMap::new(),
            additional_properties: Some(Box::new(AdditionalProperties::Schema(
                std::sync::Arc::new(JsonSchema {
                    schema_type: Some("number".to_string()),
                    ..Default::default()
                }),
            ))),
            ..Default::default()
        };
        let path = json_path::parse_path("any_key").unwrap();
        assert_eq!(resolve_leaf_type(&top, &path).as_deref(), Some("number"));
    }

    /// P1：[resolve_leaf_type] additional_properties 为 Enabled(true) 时返回 None（无类型信息）
    /// 条件：schema 有 additional_properties=Enabled(true)，key 不在 properties 中
    /// 断言：返回 None
    #[test]
    fn resolve_leaf_type_additional_properties_enabled_returns_none() {
        let top = JsonSchema {
            schema_type: Some("object".to_string()),
            properties: {
                let mut m = IndexMap::new();
                m.insert(
                    "a".to_string(),
                    std::sync::Arc::new(JsonSchema {
                        schema_type: Some("object".to_string()),
                        ..Default::default()
                    }),
                );
                m
            },
            additional_properties: Some(Box::new(AdditionalProperties::Enabled(true))),
            ..Default::default()
        };
        let path = json_path::parse_path("a.unknown_key").unwrap();
        assert!(resolve_leaf_type(&top, &path).is_none());
    }

    /// P1：[resolve_leaf_type] 路径中某段 key 不存在于 schema 中的 properties 时返回 None
    /// 条件：schema 无 sub 属性，查询 a.sub.type
    /// 断言：返回 None
    #[test]
    fn resolve_leaf_type_key_not_in_schema_returns_none() {
        let top = JsonSchema {
            schema_type: Some("object".to_string()),
            properties: {
                let mut m = IndexMap::new();
                m.insert(
                    "a".to_string(),
                    std::sync::Arc::new(JsonSchema {
                        schema_type: Some("object".to_string()),
                        ..Default::default()
                    }),
                );
                m
            },
            ..Default::default()
        };
        let path = json_path::parse_path("a.sub.type").unwrap();
        assert!(resolve_leaf_type(&top, &path).is_none());
    }

    // ── apply_set_ops 编排 ──

    /// P1：[apply_set_ops] 缺 `=` 时返回 Err
    /// 条件：--set a.b（无 =）
    /// 断言：返回 Err 且提示第 N 项缺 `=`
    #[test]
    fn apply_set_ops_missing_equals() {
        let mut payload = json!({});
        let err = apply_set_ops(&mut payload, &["a.b".to_string()], None).unwrap_err();
        assert!(err.to_string().contains("缺少 `=`"));
    }

    /// P0：[apply_set_ops] 多项顺序覆盖
    /// 条件：--set a=1 --set a=2
    /// 断言：最终 a==2
    #[test]
    fn apply_set_ops_sequential_override() {
        let mut payload = json!({});
        apply_set_ops(&mut payload, &["a=1".to_string(), "a=2".to_string()], None).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!({"a": 2}));
    }

    /// P0：[apply_set_ops] 覆盖 --json 同名深层值
    /// 条件：json {"a":{"b":1}} + --set a.b=9
    /// 断言：a.b==9
    #[test]
    fn apply_set_ops_overrides_json_deep() {
        let mut payload = json!({"a": {"b": 1}});
        apply_set_ops(&mut payload, &["a.b=9".to_string()], None).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!({"a": {"b": 9}}));
    }

    /// P1：[apply_set_ops] RHS 含 `=` 不再被切分
    /// 条件：--set note=k=v
    /// 断言：note=="k=v"（按第一个 = 切分）
    #[test]
    fn apply_set_ops_rhs_contains_equals() {
        let mut payload = json!({});
        apply_set_ops(&mut payload, &["note=k=v".to_string()], None).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!({"note": "k=v"}));
    }

    /// P2：[apply_set_ops] 类型冲突错误只包一层且带完整参数路径与原因
    /// 条件：payload={"a":1}，--set a.b.c=9（a 为标量，无法在其下建 object）
    /// 断言：消息含 "--set a.b.c" 参数标识与 "目标节点不是对象" 原因，
    ///       且不出现 "ValidationError:" / "[code=" 的重复包裹痕迹
    #[test]
    fn apply_set_ops_conflict_error_is_complete_and_not_double_wrapped() {
        let mut payload = json!({"a": 1});
        let err = apply_set_ops(&mut payload, &["a.b.c=9".to_string()], None).unwrap_err();
        let msg = err.message();
        assert!(msg.contains("--set a.b.c"), "应指明出错参数，got: {msg}");
        assert!(
            msg.contains("目标节点不是对象"),
            "应说明具体原因，got: {msg}"
        );
        assert!(
            !msg.contains("ValidationError:") && !msg.contains("[code="),
            "不应重复包裹内层 Error 的 Display，got: {msg}"
        );
    }

    /// P2：[apply_set_ops] 值定型失败错误只包一层且带完整参数路径与原因
    /// 条件：schema 定义 n 为 integer，--set n=abc（无法解析为数字）
    /// 断言：消息含 "--set n" 参数标识与 "不是有效的数值" 原因，
    ///       且不出现内层 Error 的 "ValidationError:" / "[code=" 重复包裹
    #[test]
    fn apply_set_ops_value_error_is_complete_and_not_double_wrapped() {
        let mut payload = json!({});
        let schema = schema_with_typed(&[("n", "integer")]);
        let err = apply_set_ops(&mut payload, &["n=abc".to_string()], Some(&schema)).unwrap_err();
        let msg = err.message();
        assert!(msg.contains("--set n"), "应指明出错参数，got: {msg}");
        assert!(msg.contains("不是有效的数值"), "应说明具体原因，got: {msg}");
        assert!(
            !msg.contains("ValidationError:") && !msg.contains("[code="),
            "不应重复包裹内层 Error 的 Display，got: {msg}"
        );
    }

    // ── apply_extras ──

    /// 构造一个 object 类型 schema，properties 含给定字段名（均为 string 类型）。
    fn schema_with(keys: &[&str]) -> JsonSchema {
        let mut properties = indexmap::IndexMap::new();
        for &k in keys {
            properties.insert(
                k.to_string(),
                std::sync::Arc::new(JsonSchema {
                    schema_type: Some("string".to_string()),
                    ..Default::default()
                }),
            );
        }
        JsonSchema {
            schema_type: Some("object".to_string()),
            properties,
            ..Default::default()
        }
    }

    /// 构造一个 object 类型 schema，properties 为给定的 (字段名, type) 对。
    fn schema_with_typed(fields: &[(&str, &str)]) -> JsonSchema {
        let mut properties = indexmap::IndexMap::new();
        for &(k, ty) in fields {
            properties.insert(
                k.to_string(),
                std::sync::Arc::new(JsonSchema {
                    schema_type: Some(ty.to_string()),
                    ..Default::default()
                }),
            );
        }
        JsonSchema {
            schema_type: Some("object".to_string()),
            properties,
            ..Default::default()
        }
    }

    /// P0：[apply_extras] dry_run 布尔标志提取
    /// 条件：schema 为空，payload 为 {"dry_run":true}
    /// 断言：payload 变为 {}，args.dry_run 为 Some(true)
    #[test]
    fn extract_dry_run_bool() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"dry_run": true});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.dry_run, Some(true));
    }

    /// P0：[apply_extras] dry_run 在 from_arg_matches 真实状态下提取
    /// 条件：args 模拟 from_arg_matches 产物（page_delay=Some(100)），
    ///       schema 为 {"id":"string"}，payload 为 {"dry_run":true}
    /// 断言：payload 变为 {}，args.dry_run 为 Some(true)
    #[test]
    fn extract_dry_run_bool_with_real_args_state() {
        let mut args = MethodCmdArgs {
            page_delay: Some(100),
            ..Default::default()
        };
        let mut payload = json!({"dry_run": true});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&["id"]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.dry_run, Some(true));
    }

    /// P0：[apply_extras] 非 schema 的 arg 字段解析成功后移除并写入 args
    /// 条件：schema 为空，payload 为 {"page_count":10}
    /// 断言：payload 变为 {}，args.page_count 为 Some(10)
    #[test]
    fn extract_good_arg_key_is_applied_and_removed() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"page_count": 10});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.page_count, Some(10));
    }

    /// P0：[apply_extras] 不覆盖 already set 的 args — merge 而不是 replace
    /// 条件：CLI 已设 page_count=Some(5)，JSON 只含 page_delay=200
    /// 断言：page_count 保持 Some(5)，page_delay 写入 Some(200)
    #[test]
    fn extract_merges_without_overwriting_existing_args() {
        let mut args = MethodCmdArgs {
            page_count: Some(5),
            ..Default::default()
        };
        let mut payload = json!({"page_delay": 200});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.page_count, Some(5), "existing page_count must survive");
        assert_eq!(args.page_delay, Some(200));
    }

    /// P0：[apply_extras] 同名字段冲突时 args 自身值优先，JSON 键保留在 payload
    /// 条件：CLI 已设 page_count=Some(5)，JSON 同名含 page_count=99
    /// 断言：args.page_count 保持 Some(5)，payload 保留 {"page_count":99}
    #[test]
    fn extract_existing_arg_wins_over_json_on_conflict() {
        let mut args = MethodCmdArgs {
            page_count: Some(5),
            ..Default::default()
        };
        let mut payload = json!({"page_count": 99});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_eq!(args.page_count, Some(5), "target value must win over JSON");
        assert_json_diff::assert_json_eq!(payload, json!({"page_count": 99}));
    }

    /// P1：[apply_extras] 候选 key 类型不符解析失败时保留在 payload，args 不变
    /// 条件：schema 为空，payload 为 {"page_count":"abc"}（u32 无法解析字符串）
    /// 断言：payload 保留 {"page_count":"abc"}，args.page_count 为 None
    #[test]
    fn extract_bad_typed_key_is_kept() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"page_count": "abc"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"page_count": "abc"}));
        assert_eq!(args.page_count, None);
    }

    /// P1：[apply_extras] schema 定义了与 arg 同名的字段时不提取，保留在 payload
    /// 条件：schema 定义 page_count，payload 为 {"page_count":10}
    /// 断言：payload 保留 {"page_count":10}，args.page_count 为 None
    #[test]
    fn extract_schema_defined_arg_name_is_not_extracted() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"page_count": 10});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&["page_count"]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"page_count": 10}));
        assert_eq!(args.page_count, None);
    }

    /// P1：[apply_extras] 混合场景 —— schema 保留、good 提取、bad 保留
    /// 条件：schema 定义 userid，payload 含 userid（schema）、page_count=5（good）、
    ///       dry_run="x"（bad，非 bool）
    /// 断言：payload 保留 userid 与 dry_run，args.page_count=Some(5)，args.dry_run=None
    #[test]
    fn extract_mixed_schema_good_and_bad() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"userid": "u1", "page_count": 5, "dry_run": "x"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&["userid"]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"userid": "u1", "dry_run": "x"}));
        assert_eq!(args.page_count, Some(5));
        assert_eq!(args.dry_run, None);
    }

    // ── key 变体归一化 ──

    /// P1：[apply_extras] 四种 key 变体都归一化并被提取
    /// 条件：分别以 page_count / page-count / --page_count / --page-count 为 key
    /// 断言：每种变体都写入 args.page_count=Some(7)，payload 变为 {}
    #[test]
    fn extract_normalizes_all_key_variants() {
        for key in ["page_count", "page-count", "--page_count", "--page-count"] {
            let mut args = MethodCmdArgs::default();
            let mut payload = json!({ key: 7 });
            apply_extras(
                &mut args,
                payload.as_object_mut().unwrap(),
                &schema_with(&[]),
            );
            assert_json_diff::assert_json_eq!(payload, json!({}));
            assert_eq!(args.page_count, Some(7), "variant {key} should apply");
        }
    }

    /// P1：[apply_extras] 变体 key 解析失败时保留**原始 key** 而非归一化形态
    /// 条件：schema 为空，payload 为 {"--page-count":"abc"}（u32 无法解析字符串）
    /// 断言：payload 原样保留 {"--page-count":"abc"}，args.page_count 为 None
    #[test]
    fn extract_bad_variant_key_keeps_original_spelling() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"--page-count": "abc"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"--page-count": "abc"}));
        assert_eq!(args.page_count, None);
    }

    /// P1：[apply_extras] 变体 key 与已设字段冲突时保留**原始 key** 而非归一化形态
    /// 条件：CLI 已设 page_count=Some(5)，payload 为 {"--page-count":99}
    /// 断言：args.page_count 保持 Some(5)，payload 保留 {"--page-count":99}
    #[test]
    fn extract_conflicting_variant_key_keeps_original_spelling() {
        let mut args = MethodCmdArgs {
            page_count: Some(5),
            ..Default::default()
        };
        let mut payload = json!({"--page-count": 99});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_eq!(args.page_count, Some(5), "target value must win over JSON");
        assert_json_diff::assert_json_eq!(payload, json!({"--page-count": 99}));
    }

    // ── HelperCmdArgs 路径 ──

    /// P0：[apply_extras] 提取 help 布尔标志
    /// 条件：schema 为空，payload 为 {"help":true}
    /// 断言：payload 变为 {}，args.help 为 Some(true)
    #[test]
    fn helper_extract_help_flag() {
        let mut args = HelperCmdArgs::default();
        let mut payload = json!({"help": true});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.help, Some(true));
    }

    /// P0：[apply_extras] args 不识别的字段保留在 payload（HelperCmdArgs 路径）
    /// 条件：HelperCmdArgs 无 output_dir 字段，payload 为 {"output_dir":"/tmp/out"}
    /// 断言：payload 保留 output_dir，args 不变（help 仍为 None）
    #[test]
    fn helper_extract_unknown_field_is_kept() {
        let mut args = HelperCmdArgs::default();
        let mut payload = json!({"output_dir": "/tmp/out"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"output_dir": "/tmp/out"}));
        assert_eq!(args.help, None);
    }

    /// P1：[apply_extras] payload 为空 object 时无候选，提前返回
    /// 条件：payload 为 {}，schema 为空
    /// 断言：payload 仍为 {}，args 未改动
    #[test]
    fn extract_empty_object_early_returns() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
        assert_eq!(args.page_count, None);
    }

    /// P1：[apply_extras] payload 仅含 schema 定义的字段时全部保留，无候选
    /// 条件：schema 定义 userid，payload 为 {"userid":"u1"}
    /// 断言：payload 保留 userid，args.page_count 为 None
    #[test]
    fn extract_only_schema_keys_are_kept() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"userid": "u1"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&["userid"]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"userid": "u1"}));
        assert_eq!(args.page_count, None);
    }

    // ── Unknown field in MethodCmdArgs ──

    /// P1：[apply_extras] args 不识别的字段保留在 payload（MethodCmdArgs 路径）
    /// 条件：payload 含 unknown_key（MethodCmdArgs 无此字段）
    /// 断言：payload 保留 unknown_key，args 不变（page_count 仍为 None）
    #[test]
    fn method_extract_unknown_field_is_kept() {
        let mut args = MethodCmdArgs::default();
        let mut payload = json!({"unknown_key": "val"});
        apply_extras(
            &mut args,
            payload.as_object_mut().unwrap(),
            &schema_with(&[]),
        );
        assert_json_diff::assert_json_eq!(payload, json!({"unknown_key": "val"}));
        assert_eq!(args.page_count, None);
    }

    // ── normalize_extra_key ──

    /// P1：[normalize_extra_key] 去除 `--` 前缀并把连字符转下划线
    /// 条件：分别输入带前缀、纯连字符、纯下划线、无分隔的 key
    /// 断言：全部归一化为 snake_case 且无前缀
    #[test]
    fn normalize_key_covers_all_forms() {
        assert_eq!(normalize_extra_key("--page-count"), "page_count");
        assert_eq!(normalize_extra_key("--page_count"), "page_count");
        assert_eq!(normalize_extra_key("page-count"), "page_count");
        assert_eq!(normalize_extra_key("page_count"), "page_count");
        assert_eq!(normalize_extra_key("a-b-c"), "a_b_c");
        assert_eq!(normalize_extra_key("plain"), "plain");
    }

    // ── non-object payload（assemble_payload 的非 object 守卫）──

    /// P1：[assemble_payload] --json 体为非 object（数组）时跳过 extras 抽取
    /// 条件：--json 为 "[1,2,3]"，schema 为空 object，无 CLI flag
    /// 断言：payload 原样保留为 [1,2,3]，args.page_count 仍为 None
    #[test]
    fn assemble_non_object_json_skips_extras() {
        let mut args = MethodCmdArgs {
            json: Some("[1,2,3]".to_string()),
            ..Default::default()
        };
        let schema = schema_with(&[]);
        let cmd = clap::Command::new("test");
        let matches = cmd.try_get_matches_from(vec!["test"]).unwrap();
        let payload = assemble_payload(&mut args, Some(&schema), &matches).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!([1, 2, 3]));
        assert_eq!(args.page_count, None);
    }

    /// P1：[assemble_payload] --json 体为非 object（字符串）时跳过 extras 抽取
    /// 条件：--json 为 "\"x\""，schema 为空 object，无 CLI flag
    /// 断言：payload 原样保留为 "x"，args.help 仍为 None
    #[test]
    fn assemble_non_object_string_json_skips_extras() {
        let mut args = HelperCmdArgs {
            json: Some(r#""x""#.to_string()),
            ..Default::default()
        };
        let schema = schema_with(&[]);
        let cmd = clap::Command::new("test");
        let matches = cmd.try_get_matches_from(vec!["test"]).unwrap();
        let payload = assemble_payload(&mut args, Some(&schema), &matches).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!("x"));
        assert_eq!(args.help, None);
    }

    // ── from_arg_matches 集成 ──

    /// P0：[apply_extras] clap from_arg_matches → extract 对 dry_run 的端到端
    /// 条件：用 clap 解析 ["--json", "{\"dry_run\": true}", "--id", "root"]，
    ///       schema 为 {"id":"string"}
    /// 断言：args.dry_run == Some(true)，payload 变为 {"id":"root"}
    #[test]
    fn dry_run_extraction_via_clap_from_arg_matches() {
        use clap::{Args as _, FromArgMatches};

        let list_cmd = MethodCmdArgs::augment_args(
            clap::Command::new("list").disable_help_flag(true).arg(
                clap::Arg::new("id")
                    .long("id")
                    .value_parser(clap::value_parser!(String)),
            ),
        );

        let cmd = clap::Command::new("test").subcommand(list_cmd);

        let matches = cmd
            .try_get_matches_from([
                "test",
                "list",
                "--id",
                "root",
                "--json",
                r#"{"dry_run": true}"#,
            ])
            .unwrap();

        let cmd_matches = matches.subcommand_matches("list").unwrap();

        let mut args = MethodCmdArgs::from_arg_matches(cmd_matches).expect("from_arg_matches");

        assert!(
            args.json.as_deref() == Some(r#"{"dry_run": true}"#),
            "args.json should be set, got: {:?}",
            args.json
        );

        assert_eq!(
            args.dry_run,
            Some(false),
            "clap fills dry_run=Some(false) when flag absent"
        );

        let mut payload: Value = serde_json::from_str::<Value>(r#"{"dry_run": true}"#).unwrap();
        let schema = schema_with(&["id"]);
        apply_extras(&mut args, payload.as_object_mut().unwrap(), &schema);

        assert_eq!(
            args.dry_run,
            Some(true),
            "dry_run should be Some(true) after extraction"
        );
        assert_json_diff::assert_json_eq!(payload, json!({}));
    }

    // ── repair_json ──

    /// P0：[repair_json] 合法 JSON 直接解析成功
    /// 条件：输入标准 JSON {"a":1}
    /// 断言：返回 Value {"a":1}
    #[test]
    fn repair_json_valid() {
        let v = repair_json(r#"{"a":1}"#).unwrap();
        assert_json_diff::assert_json_eq!(v, json!({"a": 1}));
    }

    /// P1：[repair_json] 缺失引号的键名经 jsonrepair 修复后成功
    /// 条件：输入非标准 JSON {a:1}（键名无引号）
    /// 断言：修复后返回 Value {"a":1}
    #[test]
    fn repair_json_repaired() {
        let v = repair_json(r#"{a:1}"#).unwrap();
        assert_json_diff::assert_json_eq!(v, json!({"a": 1}));
    }

    /// P1：[repair_json] 无法修复的输入返回 Err
    /// 条件：输入不可修复的残缺 JSON 如 "{:]"（花括号方括号混用）
    /// 断言：返回 Err
    #[test]
    fn repair_json_unrepairable() {
        assert!(repair_json("{:]").is_err());
    }

    // ── repair_json 遥测 ──

    /// P1：[repair_json] 合法 JSON 不发射遥测事件（非 repair 路径不上报）
    /// 条件：输入合法 JSON
    /// 断言：CaptureScope 没有收到任何事件
    #[test]
    fn repair_json_telemetry_no_event_on_valid() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let _ = repair_json(r#"{"a":1}"#);
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert!(snaps.is_empty(), "合法 JSON 不应发射遥测事件");
    }

    /// P1：[repair_json] 修复后 JSON 发射 ok_repaired 遥测事件
    /// 条件：输入可修复的非标准 JSON
    /// 断言：CaptureScope 收到 kind="json_repair"、outcome="ok_repaired" 的事件，
    ///       且 payload 附带修复前后 JSON（input=原文，output=修复后序列化）
    #[test]
    fn repair_json_telemetry_ok_repaired() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let _ = repair_json(r#"{a:1}"#);
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].kind, ctr::json_repair::KIND);
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::json_repair::FIELD_OUTCOME],
            json!(ctr::json_repair::OUTCOME_OK_REPAIRED)
        );
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::json_repair::FIELD_INPUT],
            json!(r#"{a:1}"#)
        );
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::json_repair::FIELD_OUTPUT],
            json!(r#"{"a":1}"#)
        );
    }

    /// P1：[repair_json] 修复失败时发射 err_repair 遥测事件
    /// 条件：输入不可修复的残缺 JSON
    /// 断言：CaptureScope 收到 kind="json_repair"、outcome="err_repair" 的事件，
    ///       且 payload 附带修复前原文（input）
    #[test]
    fn repair_json_telemetry_err_repair() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let _ = repair_json("{:]");
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].kind, ctr::json_repair::KIND);
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::json_repair::FIELD_OUTCOME],
            json!(ctr::json_repair::OUTCOME_ERR_REPAIR)
        );
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::json_repair::FIELD_INPUT],
            json!("{:]")
        );
    }

    // ── apply_set_ops 遥测 ──

    /// P1：[apply_set_ops] 成功时发射 set_path 事件并携带 count 和 typed_by_schema
    /// 条件：--set a=1 --set b=2，无 schema
    /// 断言：kind="set_path"、outcome="ok"、count=2、typed_by_schema=0
    #[test]
    fn apply_set_ops_telemetry_ok() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let mut payload = json!({});
        let _ = apply_set_ops(&mut payload, &["a=1".to_string(), "b=2".to_string()], None);
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].kind, ctr::set_path::KIND);
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::set_path::FIELD_OUTCOME],
            json!(ctr::set_path::OUTCOME_OK)
        );
        assert_json_diff::assert_json_eq!(snaps[0].payload[ctr::set_path::FIELD_COUNT], json!(2));
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::set_path::FIELD_TYPED_BY_SCHEMA],
            json!(0)
        );
    }

    /// P1：[apply_set_ops] 失败时发射 set_path 事件并携带 count
    /// 条件：--set a.b（缺 =）
    /// 断言：kind="set_path"、outcome="err"
    #[test]
    fn apply_set_ops_telemetry_err() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let mut payload = json!({});
        let _ = apply_set_ops(&mut payload, &["a.b".to_string()], None);
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].kind, ctr::set_path::KIND);
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::set_path::FIELD_OUTCOME],
            json!(ctr::set_path::OUTCOME_ERR)
        );
    }

    /// P1：[apply_set_ops] 有 schema 时 typed_by_schema 反映策略 A 使用次数
    /// 条件：schema 定义 x 类型为 string；--set x=hello --set y[0]=1
    /// 断言：typed_by_schema=1（仅 x 走了策略 A）
    #[test]
    fn apply_set_ops_telemetry_typed_by_schema() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::Registry::default().with(TelemetryLayer::new()),
        );

        let collected: Arc<Mutex<Vec<ClientEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c = collected.clone();

        let scope = CaptureScope::new();
        scope.on_event(move |ev: ClientEvent| {
            c.lock().unwrap().push(ev);
        });

        let _enter = scope.span().enter();
        let mut payload = json!({});
        let schema = schema_with(&["x"]);
        let _ = apply_set_ops(
            &mut payload,
            &["x=hello".to_string(), "y[0]=1".to_string()],
            Some(&schema),
        );
        drop(_enter);

        let snaps: Vec<ClientEvent> = std::mem::take(&mut *collected.lock().unwrap());
        assert_eq!(snaps.len(), 1);
        assert_json_diff::assert_json_eq!(
            snaps[0].payload[ctr::set_path::FIELD_TYPED_BY_SCHEMA],
            json!(1)
        );
    }

    // ── resolve_set_value P2 边界 ──

    /// P2：[resolve_set_value] 策略 A：叶子 array，RHS 为 lenient JSON
    /// 条件：leaf_type 为 "array"，输入 "[1,2,3]"
    /// 断言：返回 [1,2,3]
    #[test]
    fn resolve_set_value_array_type() {
        let v = resolve_set_value("[1,2,3]", Some("array")).unwrap();
        assert_json_diff::assert_json_eq!(v, json!([1, 2, 3]));
    }

    /// P2：[resolve_set_value] 策略 A：未知 schema 类型回退到策略 B 推断
    /// 条件：leaf_type 为 "custom"，输入 "42"
    /// 断言：回退 B，解析为 Number(42)
    #[test]
    fn resolve_set_value_unknown_schema_falls_back() {
        let v = resolve_set_value("42", Some("custom")).unwrap();
        assert_eq!(v, json!(42));
    }

    // ── assemble_payload 无 schema ──

    /// P1：[assemble_payload] schema 为 None 时仅解析 --json 并应用 --set
    /// 条件：--json {"a":1}，--set a=2，schema=None
    /// 断言：最终 payload 为 {"a":2}
    #[test]
    fn assemble_payload_no_schema() {
        let mut args = MethodCmdArgs {
            json: Some(r#"{"a":1}"#.to_string()),
            set: vec!["a=2".to_string()],
            ..Default::default()
        };
        let cmd = clap::Command::new("test");
        let matches = cmd.try_get_matches_from(vec!["test"]).unwrap();
        let payload = assemble_payload(&mut args, None, &matches).unwrap();
        assert_json_diff::assert_json_eq!(payload, json!({"a": 2}));
    }

    // ── matches_to_value 数组 ──

    /// P1：[matches_to_value] 对 array 类型并有多值输入时正确收集为 JSON 数组
    /// 条件：schema 定义 tags 为 array，items type 为 string，
    ///       传入 --tags a --tags b
    /// 断言：结果包含 "tags": ["a", "b"]
    #[test]
    fn matches_to_value_array_with_multiple_values() {
        let mut schema = JsonSchema {
            schema_type: Some("object".into()),
            ..Default::default()
        };
        let item_schema = JsonSchema {
            schema_type: Some("string".into()),
            ..Default::default()
        };
        let prop = JsonSchema {
            schema_type: Some("array".into()),
            items: Some(std::sync::Arc::new(item_schema)),
            ..Default::default()
        };
        schema
            .properties
            .insert("tags".to_string(), std::sync::Arc::new(prop));

        let cmd = clap::Command::new("test").arg(
            clap::Arg::new("tags")
                .long("tags")
                .value_parser(clap::value_parser!(String))
                .action(clap::ArgAction::Append),
        );
        let matches = cmd
            .try_get_matches_from(["test", "--tags", "a", "--tags", "b"])
            .unwrap();

        let result = matches_to_value(&schema, &matches).unwrap();
        assert!(
            result.contains_key("tags"),
            "should contain 'tags' key, got: {result:?}"
        );
        assert_json_diff::assert_json_eq!(&result["tags"], json!(["a", "b"]));
    }
}
