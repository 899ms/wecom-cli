use rand::RngExt;

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
    //!
    //! ### 关键分支与异常路径
    //! - len = 0 → empty string
    //! - len > 0 → string of exact length, all chars alphanumeric
    //! - Two calls → distinct strings (probabilistic)
    //!
    //! ### 上下游交互
    //! - 上游：client::builder (generate_random_id), directive::file_save (random_file_name), fs (collision suffix)
    //! - 下游：rand crate

    use super::*;

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
