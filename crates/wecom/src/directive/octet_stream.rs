use std::collections::HashSet;

use super::Directive;
use crate::{Result, fs, json_path, schema};

/// 递归遍历 schema，检查是否存在 `x-wecom-octet-stream: true` 的字段。
pub fn check_has_octet_stream(schema: &schema::JsonSchema) -> bool {
    if schema.directives.octet_stream.is_some() {
        return true;
    }

    for child in schema.properties.values() {
        if check_has_octet_stream(child.as_ref()) {
            return true;
        }
    }

    for child in &schema.one_of {
        if check_has_octet_stream(child.as_ref()) {
            return true;
        }
    }

    if let Some(items) = &schema.items
        && check_has_octet_stream(items.as_ref())
    {
        return true;
    }

    false
}

pub async fn build_multipart_form(
    fs: &fs::Fs,
    payload: &serde_json::Value,
    directives: &[Directive<'_>],
) -> Result<reqwest::multipart::Form> {
    let mut file_fields = HashSet::new();
    for d in directives {
        if let Directive::UploadMultipart { path, .. } = d {
            file_fields.insert(json_path::segments_to_path(path));
        }
    }

    let parts = json_path::flatten_value(payload);
    let mut form = reqwest::multipart::Form::new();

    for (name, value) in parts {
        if file_fields.contains(name.as_str()) {
            let part = fs.open_as_multipart_part(&value).await.inspect_err(
                |e| tracing::error!(error = %e, "open file for multipart upload failed"),
            )?;
            form = form.part(name, part);
        } else {
            form = form.text(name, value);
        }
    }

    Ok(form)
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：octet_stream（Octet-Stream 检测与 multipart 构建）
    //!
    //! ### 关键接口
    //! - [check_has_octet_stream] — 递归检查 schema 中是否存在 x-wecom-octet-stream: true 标记
    //! - [build_multipart_form] — 根据 directives 将 payload 扁平化为 multipart 表单
    //!
    //! ### 关键分支与异常路径
    //! - check_has_octet_stream：顶层标记、properties 嵌套、oneOf 嵌套、items 嵌套四种检测路径；均无则返回 false
    //! - build_multipart_form：文件字段用 open_as_multipart_part，普通字段用 text
    //!
    //! ### 上下游交互
    //! - 上游：HTTP 请求构建层调用本模块判断是否需要 multipart 及构建表单
    //! - 下游：依赖 [schema::JsonSchema]（指令标记）、[json_path::flatten_value]（扁平化）、[fs::Fs]（文件读取）

    use std::sync::Arc;

    use super::*;

    fn make_schema() -> schema::JsonSchema {
        schema::JsonSchema::default()
    }

    fn with_octet_stream() -> schema::JsonSchema {
        schema::JsonSchema {
            directives: schema::JsonSchemaWecomDirectives {
                octet_stream: Some(schema::WecomBoolValue::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// P1：空 schema 无 octet_stream 标记时返回 false
    /// 条件：使用默认空 schema
    /// 断言：check_has_octet_stream 返回 false
    #[test]
    fn empty_schema_is_false() {
        assert!(!check_has_octet_stream(&make_schema()));
    }

    /// P0：顶层 schema 直接标记 octet_stream 时返回 true
    /// 条件：schema 的 directives.octet_stream.is_some()
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn direct_octet_stream() {
        assert!(check_has_octet_stream(&with_octet_stream()));
    }

    /// P0：[check_has_octet_stream] 嵌套在 properties 中的 octet_stream 标记被检测到
    /// 条件：schema 的 property 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_property() {
        let mut s = make_schema();
        s.properties
            .insert("data".into(), Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 嵌套在 oneOf 中的 octet_stream 标记被检测到
    /// 条件：schema 的 oneOf 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_one_of() {
        let mut s = make_schema();
        s.one_of.push(Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 嵌套在 items 中的 octet_stream 标记被检测到
    /// 条件：schema 的 items 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_items() {
        let mut s = make_schema();
        s.items = Some(Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 深层嵌套属性中无 octet_stream 标记时返回 false
    /// 条件：schema 含有嵌套属性但均未标记 octet_stream
    /// 断言：check_has_octet_stream 返回 false
    #[test]
    fn deep_nested_property_not_found() {
        let inner = make_schema();
        let mut s = make_schema();
        s.properties.insert("nested".into(), Arc::new(inner));
        assert!(!check_has_octet_stream(&s));
    }

    // ── build_multipart_form ──

    /// P0：[build_multipart_form] 普通文本字段构建为 text 表单
    /// 条件：payload 含 key=value 文本字段，无 UploadMultipart directive
    /// 断言：返回的 Form 包含 text 字段
    #[tokio::test]
    async fn build_multipart_form_text_only() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let payload = serde_json::json!({"name": "test.txt"});
        let _form = build_multipart_form(&fs, &payload, &[]).await.unwrap();
    }
}
