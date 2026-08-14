//! JSON repair 提示监听：基于统一 telemetry 事件，在 json repair 成功时
//! 向 stderr 输出修复前后 JSON，帮助用户核对被自动修复的请求体。

use wecom::telemetry::{ClientEvent, EventExt};
use wecom_transport::telemetry::CaptureScope;

/// `json_repair` 事件 kind（与 `wecom::telemetry::contract::json_repair` 对应）。
const KIND_JSON_REPAIR: &str = "json_repair";
/// 事件 payload 中的 outcome 字段。
const FIELD_OUTCOME: &str = "outcome";
/// outcome = ok_repaired：修复成功。
const OUTCOME_OK_REPAIRED: &str = "ok_repaired";
/// 修复前（原始输入）JSON 字段。
const FIELD_INPUT: &str = "input";
/// 修复后 JSON 字段。
const FIELD_OUTPUT: &str = "output";

/// 注册 json repair 成功提示监听。
///
/// 监听挂在给定的 [`CaptureScope`] 上；scope 的 span 需覆盖 CLI 主流程
/// （如 `main` 中 attach 到 root span），`json_repair` 事件才会被捕获。
/// 仅处理 `outcome=ok_repaired` 的事件（修复失败由调用方报错，无需提示），
/// 其余静默忽略。
pub fn install_json_repair_listener(scope: &CaptureScope) {
    scope.on_event(|ev: ClientEvent| {
        if ev.kind != KIND_JSON_REPAIR {
            return;
        }
        if ev
            .payload
            .get(FIELD_OUTCOME)
            .and_then(serde_json::Value::as_str)
            != Some(OUTCOME_OK_REPAIRED)
        {
            return;
        }

        let before = ev
            .payload
            .get(FIELD_INPUT)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let after_raw = ev
            .payload
            .get(FIELD_OUTPUT)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        eprintln!("{}", format_json_repair_hint(before, after_raw));
    });
}

/// 组装 stderr 提示文本：修复前原文 + 修复后 pretty JSON。
fn format_json_repair_hint(before: &str, after_raw: &str) -> String {
    format!(
        "[wecom] json repair: 输入 JSON 已自动修复\n--- 修复前 ---\n{before}\n--- 修复后 ---\n{}",
        pretty_json(after_raw)
    )
}

/// 将紧凑 JSON 字符串转为 pretty 形式；解析失败时原样返回。
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0：[format_json_repair_hint] 提示文本包含修复前后 JSON，修复后为 pretty 形式
    #[test]
    fn hint_contains_before_and_pretty_after() {
        let hint = format_json_repair_hint(r#"{bad: "value"}"#, r#"{"bad":"value"}"#);
        assert!(hint.contains("json repair"), "应含修复提示，got: {hint}");
        assert!(
            hint.contains(r#"{bad: "value"}"#),
            "应含修复前原文，got: {hint}"
        );
        assert!(
            hint.contains("  \"bad\": \"value\""),
            "修复后应为 pretty JSON，got: {hint}"
        );
    }

    /// P1：[pretty_json] 非法 JSON 原样返回
    #[test]
    fn pretty_json_invalid_returns_original() {
        assert_eq!(pretty_json("not json"), "not json");
    }
}
