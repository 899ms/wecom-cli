use rand::RngExt;

/// 将 lib 层错误转换为 transport 层错误：`Transport` 变体拆包保留原语义，
/// 其余变体装箱为 `Error::Other` 透传（供 multipart 再次构建闭包使用）。
///
/// 装箱不是单向有损的：错误回流 `crate::Error` 时，
/// `From<wecom_transport::Error> for crate::Error` 会将 `Other` 内的
/// `crate::Error` 负载 downcast 还原，用户可见的 code / type / message
/// 元信息不会丢失。
pub(crate) fn to_transport_error(e: crate::Error) -> wecom_transport::Error {
    match e {
        crate::Error::Transport(t) => t,
        other => wecom_transport::Error::Other(Box::new(other)),
    }
}

/// Generate a random alphanumeric string of the given length.
pub(crate) fn random_str(len: usize) -> String {
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(len)
        .map(|b| b as char)
        .collect()
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：util（通用工具函数）
    //!
    //! ### 关键接口
    //! - [random_str] — Generate a random alphanumeric string of the given length
    //! - [to_transport_error] — lib 层错误 → transport 层错误（Transport 拆包，其余装箱 Other）
    //!
    //! ### 关键分支与异常路径
    //! - len = 0 → empty string
    //! - len > 0 → string of exact length, all chars alphanumeric
    //! - Two calls → distinct strings (probabilistic)
    //! - to_transport_error：Transport 变体 → 拆包透传；其余变体 → 装箱为 transport::Other
    //!   （回流 crate::Error 时经 From downcast 还原，见 error.rs 测试）
    //!
    //! ### 上下游交互
    //! - 上游：client::builder (generate_random_id), directive::file_save (random_file_name), fs (collision suffix)；
    //!   service::execute / builtins::upload_media / directive::octet_stream (to_transport_error)
    //! - 下游：rand crate；wecom_transport::Error

    use super::*;

    /// P0：[to_transport_error] Transport 变体拆包透传，不再装箱
    /// 条件：输入 Error::Transport(Http)
    /// 断言：返回 wecom_transport::Error::Http（原变体保留）
    #[test]
    fn to_transport_error_unwraps_transport_variant() {
        let e = crate::Error::Transport(wecom_transport::Error::Http {
            message: "not found".into(),
            endpoint: "https://example.com/api".into(),
            status: 404,
        });
        let t = to_transport_error(e);
        assert!(matches!(
            t,
            wecom_transport::Error::Http { status: 404, .. }
        ));
    }

    /// P0：[to_transport_error] 非 Transport 变体装箱为 Other 且负载可 downcast 还原
    /// 条件：输入 Error::Validation
    /// 断言：返回 wecom_transport::Error::Other，内层负载 downcast::<crate::Error> 成功且为 Validation
    #[test]
    fn to_transport_error_boxes_non_transport_variant() {
        let t = to_transport_error(crate::Error::Validation("field required".into()));
        let wecom_transport::Error::Other(inner) = t else {
            panic!("expected Other, got: {t:?}");
        };
        let recovered = inner
            .downcast::<crate::Error>()
            .expect("payload should downcast back to crate::Error");
        assert!(matches!(*recovered, crate::Error::Validation(_)));
    }

    /// P0：[random_str] returns a string of the exact requested length
    /// 条件：len = 6
    /// 断言：returned string length == 6
    #[test]
    fn random_str_exact_length() {
        let s = random_str(6);
        assert_eq!(s.len(), 6);
    }

    /// P0：[random_str] returns only alphanumeric characters
    /// 条件：len = 100 (large sample to increase confidence)
    /// 断言：every character is ASCII alphanumeric
    #[test]
    fn random_str_all_alphanumeric() {
        let s = random_str(100);
        assert!(
            s.chars().all(|c| c.is_ascii_alphanumeric()),
            "expected all alphanumeric, got: {s}"
        );
    }

    /// P1：[random_str] returns empty string when len is 0
    /// 条件：len = 0
    /// 断言：returned string is empty
    #[test]
    fn random_str_zero_length() {
        let s = random_str(0);
        assert!(s.is_empty());
    }

    /// P1：[random_str] produces distinct strings on successive calls
    /// 条件：two calls with len = 32
    /// 断言：the two strings differ (collision probability ≈ 0 for 32 alphanumeric chars)
    #[test]
    fn random_str_uniqueness() {
        let a = random_str(32);
        let b = random_str(32);
        assert_ne!(a, b, "two random strings of length 32 should differ");
    }

    /// P1：[random_str] works for various lengths used in the codebase
    /// 条件：len ∈ {6, 8, 32} (client id suffix, fs collision suffix, file name)
    /// 断言：each returned string has the correct length and is alphanumeric
    #[test]
    fn random_str_codebase_lengths() {
        for &len in &[6, 8, 32] {
            let s = random_str(len);
            assert_eq!(s.len(), len, "length mismatch for len={len}");
            assert!(
                s.chars().all(|c| c.is_ascii_alphanumeric()),
                "non-alphanumeric char in random_str({len}): {s}"
            );
        }
    }
}
