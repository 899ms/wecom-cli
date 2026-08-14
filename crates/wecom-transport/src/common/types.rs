use std::borrow::Cow;

/// Trait for types that can be converted into `Cow<'a, serde_json::Value>`.
///
/// Accepts references (zero-copy), owned values (single move), or existing
/// `Cow` instances (passthrough, preserving Borrowed/Owned form):
///
/// - `&'a serde_json::Value` → `Cow::Borrowed`
/// - `serde_json::Value`     → `Cow::Owned`
/// - `Cow<'a, Value>`        → passthrough
pub trait IntoCowValue<'a> {
    fn into_cow_value(self) -> Cow<'a, serde_json::Value>;
}

impl<'a> IntoCowValue<'a> for &'a serde_json::Value {
    fn into_cow_value(self) -> Cow<'a, serde_json::Value> {
        Cow::Borrowed(self)
    }
}

impl<'a> IntoCowValue<'a> for serde_json::Value {
    fn into_cow_value(self) -> Cow<'a, serde_json::Value> {
        Cow::Owned(self)
    }
}

impl<'a> IntoCowValue<'a> for Cow<'a, serde_json::Value> {
    fn into_cow_value(self) -> Cow<'a, serde_json::Value> {
        self
    }
}
