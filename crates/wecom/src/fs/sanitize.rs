const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
    "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn is_illegal_char(ch: char) -> bool {
    matches!(ch, '/' | '?' | '<' | '>' | '\\' | ':' | '*' | '|' | '"')
}

fn is_control_char(ch: char) -> bool {
    let code = ch as u32;
    (code <= 0x1f) || (0x80..=0x9f).contains(&code)
}

fn is_windows_reserved(name: &str) -> bool {
    let base = name.split_once(".").map(|(base, _)| base).unwrap_or(name);
    for &reserved in WINDOWS_RESERVED {
        if base.eq_ignore_ascii_case(reserved) {
            return true;
        }
    }
    false
}

pub(super) fn sanitize_filename(input: &str, windows: bool) -> String {
    let mut name = String::new();

    for c in input.trim().chars() {
        name.push(if is_illegal_char(c) || is_control_char(c) {
            '_'
        } else {
            c
        });
    }

    if name.is_empty() {
        name.push('_');
    }

    if name.chars().all(|c| c == '.') {
        name.insert(0, '_');
    }

    if windows {
        if name.ends_with(' ') || name.ends_with('.') {
            name.push('_');
        }
        if is_windows_reserved(&name) {
            name.insert(0, '_');
        }
    }

    if name.len() > 255 {
        name.truncate(255);
    }

    name
}

// ── Content-Disposition parsing ─────────────────────────────

/// Parse an RFC 5987 ext-value: `charset'language'value-chars`.
///
/// Only UTF-8 charset is supported (which covers virtually all real-world
/// usage).  Percent-encoded octets are decoded.
fn parse_ext_value(raw: &str) -> Option<String> {
    // Format: charset'language'value-chars  (language may be empty)
    let mut parts = raw.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?; // we don't need the language tag
    let encoded = parts.next()?;

    if !charset.eq_ignore_ascii_case("UTF-8") {
        return None; // unsupported charset
    }

    percent_decode(encoded)
}

/// Decode percent-encoded bytes (e.g. `%E4%B8%AD` → UTF-8 bytes → string).
fn percent_decode(input: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16).ok()?;
            bytes.push(byte);
        } else {
            // Non-encoded characters are passed through as UTF-8.
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8(bytes).ok()
}

/// Parse a header parameter value that is either a quoted-string
/// (`"value"`, with `\\` and `\"` escapes) or a bare token.
fn parse_quoted_or_token(val: &str) -> String {
    if let Some(inner) = val.strip_prefix('"') {
        // Quoted-string: collect characters between the opening and closing
        // double-quote, honouring backslash escapes.
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break, // closing quote
                '\\' => {
                    // Escaped character – take the next one literally.
                    if let Some(escaped) = chars.next() {
                        result.push(escaped);
                    }
                }
                _ => result.push(c),
            }
        }
        result
    } else {
        // Token (unquoted): take until semicolon, whitespace, or end.
        val.split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    }
}

/// Extract a filename from a `Content-Disposition` header **value** string.
///
/// Implements RFC 6266 §4.3: `filename*` (RFC 5987 ext-value) takes
/// precedence over `filename`.
pub(super) fn content_disposition_filename(header_value: &str) -> Option<String> {
    let mut filename = None;
    let mut filename_star = None;

    // Split on ';' to iterate over parameters.  The first segment is the
    // disposition-type (e.g. "attachment") which we ignore.
    for (i, segment) in header_value.split(';').enumerate() {
        if i == 0 {
            continue; // skip disposition-type
        }
        let segment = segment.trim();
        if let Some((key, val)) = segment.split_once('=') {
            let key = key.trim();
            let val = val.trim();
            if key.eq_ignore_ascii_case("filename*") {
                filename_star = parse_ext_value(val);
            } else if key.eq_ignore_ascii_case("filename") {
                filename = Some(parse_quoted_or_token(val));
            }
        }
    }

    // RFC 6266 §4.3: filename* takes precedence over filename.
    filename_star.or(filename).filter(|s| !s.is_empty())
}

// ══════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    //! ## 模块摘要：sanitize（文件名净化与 Content-Disposition 解析）
    //!
    //! ### 关键接口
    //! - [sanitize_filename] — 净化文件名
    //! - [content_disposition_filename] — 从 Content-Disposition 头中提取文件名
    //! - [is_illegal_char] / [is_control_char] / [is_windows_reserved] — 字符检查
    //! - [parse_ext_value] / [percent_decode] / [parse_quoted_or_token] — 解析辅助
    //!
    //! ### 关键分支与异常路径
    //! - 非法字符/控制字符 → 替换为下划线
    //! - 空输入/全点号 → 前补下划线
    //! - 超长文件名 → 截断至 255 字符
    //! - Windows 模式：尾部空格移除、尾部点号追加下划线、保留名前加下划线
    //! - 解析失败（无效编码、截断） → 返回 None
    //!
    //! ### 上下游交互
    //! - 上游：`fs/mod.rs` 的 [Fs::sanitize_filename] 方法调用本模块
    //! - 下游：依赖 `std::string` 和 `std::char` 进行字符处理

    use super::*;

    // ── sanitize_filename ──

    /// P0：[sanitize_filename] 非法字符（/ ? < > \ : * | "）被替换为下划线
    /// 条件：输入包含多种非法字符的字符串
    /// 断言：非法字符全部替换为 _
    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(sanitize_filename("a/b:c*d", false), "a_b_c_d");
        assert_eq!(sanitize_filename("file<name>.txt", false), "file_name_.txt");
    }

    /// P0：[sanitize_filename] 控制字符（如 \x00、\x1f）被替换为下划线
    /// 条件：输入包含控制字符的字符串
    /// 断言：控制字符替换为 _
    #[test]
    fn sanitize_replaces_control_chars() {
        assert_eq!(sanitize_filename("hello\x00world", false), "hello_world");
        assert_eq!(sanitize_filename("a\x1fb", false), "a_b");
    }

    /// P1：[sanitize_filename] 空输入或纯空格输入返回 "_"
    /// 条件：输入 "" 或 "   "
    /// 断言：返回 "_"
    #[test]
    fn sanitize_empty_input() {
        assert_eq!(sanitize_filename("", false), "_");
        assert_eq!(sanitize_filename("   ", false), "_");
    }

    /// P1：[sanitize_filename] 全点号文件名前补下划线
    /// 条件：输入 "..." 或 "."
    /// 断言：返回 "_..." 或 "_."
    #[test]
    fn sanitize_all_dots() {
        assert_eq!(sanitize_filename("...", false), "_...");
        assert_eq!(sanitize_filename(".", false), "_.");
    }

    /// P0：[sanitize_filename] 文件名前后空格被 trim
    /// 条件：输入 "  hello  "
    /// 断言：返回 "hello"
    #[test]
    fn sanitize_trims_whitespace() {
        assert_eq!(sanitize_filename("  hello  ", false), "hello");
    }

    /// P1：[sanitize_filename] 超长文件名被截断至 255 字符
    /// 条件：输入 300 个字符的字符串
    /// 断言：输出长度为 255
    #[test]
    fn sanitize_truncates_long_names() {
        let long_name = "a".repeat(300);
        let result = sanitize_filename(&long_name, false);
        assert_eq!(result.len(), 255);
    }

    /// P0：[sanitize_filename] 合法文件名保持不变（包括中文、连字符、下划线、点号）
    /// 条件：输入正常文件名如 readme.md、日本語.txt 等
    /// 断言：输出与输入一致
    #[test]
    fn sanitize_normal_filenames_unchanged() {
        assert_eq!(sanitize_filename("readme.md", false), "readme.md");
        assert_eq!(
            sanitize_filename("my-file_v2.tar.gz", false),
            "my-file_v2.tar.gz"
        );
        assert_eq!(sanitize_filename("日本語.txt", false), "日本語.txt");
    }

    /// P1：[sanitize_filename] Windows 模式下尾部空格被移除
    /// 条件：windows=true，输入 "hello " 或 "a b" 或 "   "
    /// 断言："hello "→"hello"，"a b"不变，"   "→"_"
    #[test]
    fn sanitize_windows_trailing_space() {
        assert_eq!(sanitize_filename("hello ", true), "hello");
        assert_eq!(sanitize_filename("a b", true), "a b");
        assert_eq!(sanitize_filename("   ", true), "_");
    }

    /// P1：[sanitize_filename] Windows 模式下尾部点号追加下划线
    /// 条件：windows=true，输入 "data."
    /// 断言：返回 "data._"；windows=false 时保持不变
    #[test]
    fn sanitize_windows_trailing_dot() {
        assert_eq!(sanitize_filename("data.", true), "data._");
        assert_eq!(sanitize_filename("data.", false), "data.");
    }

    /// P1：[sanitize_filename] Windows 保留名前加下划线
    /// 条件：windows=true，输入 CON、con.txt、NUL、COM1.log、aux
    /// 断言：均以 "_" 前缀开头
    #[test]
    fn sanitize_windows_reserved_names() {
        assert_eq!(sanitize_filename("CON", true), "_CON");
        assert_eq!(sanitize_filename("con.txt", true), "_con.txt");
        assert_eq!(sanitize_filename("NUL", true), "_NUL");
        assert_eq!(sanitize_filename("COM1.log", true), "_COM1.log");
        assert_eq!(sanitize_filename("aux", true), "_aux");
    }

    /// P1：[sanitize_filename] 非 Windows 模式不处理保留名
    /// 条件：windows=false，输入 CON、con.txt、NUL
    /// 断言：输出与输入一致
    #[test]
    fn sanitize_windows_reserved_not_applied_on_unix() {
        assert_eq!(sanitize_filename("CON", false), "CON");
        assert_eq!(sanitize_filename("con.txt", false), "con.txt");
        assert_eq!(sanitize_filename("NUL", false), "NUL");
    }

    /// P1：[sanitize_filename] Windows 模式下非保留名保持不变
    /// 条件：windows=true，输入 readme.md、console 等正常名称
    /// 断言：输出与输入一致
    #[test]
    fn sanitize_windows_non_reserved_unchanged() {
        assert_eq!(sanitize_filename("readme.md", true), "readme.md");
        assert_eq!(sanitize_filename("console", true), "console");
    }

    // ── is_windows_reserved ──

    /// P0：[is_windows_reserved] Windows 保留名精确匹配检测
    /// 条件：输入 con、prn、aux、nul、com0、lpt9 等精确匹配
    /// 断言：is_windows_reserved 返回 true
    #[test]
    fn reserved_exact_match() {
        assert!(is_windows_reserved("con"));
        assert!(is_windows_reserved("prn"));
        assert!(is_windows_reserved("aux"));
        assert!(is_windows_reserved("nul"));
        assert!(is_windows_reserved("com0"));
        assert!(is_windows_reserved("lpt9"));
    }

    /// P1：[is_windows_reserved] Windows 保留名匹配忽略大小写
    /// 条件：输入 CON、Prn、AUX、NUL、Com1、LPT3
    /// 断言：is_windows_reserved 返回 true
    #[test]
    fn reserved_case_insensitive() {
        assert!(is_windows_reserved("CON"));
        assert!(is_windows_reserved("Prn"));
        assert!(is_windows_reserved("AUX"));
        assert!(is_windows_reserved("NUL"));
        assert!(is_windows_reserved("Com1"));
        assert!(is_windows_reserved("LPT3"));
    }

    /// P1：[is_windows_reserved] 带扩展名的保留名被正确识别
    /// 条件：输入 con.txt、NUL.log、COM1.dat、aux.tar.gz
    /// 断言：is_windows_reserved 返回 true
    #[test]
    fn reserved_with_extension() {
        assert!(is_windows_reserved("con.txt"));
        assert!(is_windows_reserved("NUL.log"));
        assert!(is_windows_reserved("COM1.dat"));
        assert!(is_windows_reserved("aux.tar.gz"));
    }

    /// P0：[is_windows_reserved] 正常名称不被误判为保留名
    /// 条件：输入 hello、readme.md、console、prn_data、com10 等
    /// 断言：返回 false
    #[test]
    fn not_reserved_normal_names() {
        assert!(!is_windows_reserved("hello"));
        assert!(!is_windows_reserved("readme.md"));
        assert!(!is_windows_reserved("console"));
        assert!(!is_windows_reserved("prn_data"));
        assert!(!is_windows_reserved("auxiliary"));
        assert!(!is_windows_reserved("com10"));
        assert!(!is_windows_reserved("lpt"));
        assert!(!is_windows_reserved(""));
    }

    // ── Content-Disposition parsing ──

    /// P0：[parse_quoted_or_token] 简单带引号字符串解析为内部值
    /// 条件：输入 "\"report.pdf\""
    /// 断言：返回 "report.pdf"
    #[test]
    fn quoted_simple() {
        assert_eq!(parse_quoted_or_token("\"report.pdf\""), "report.pdf");
    }

    /// P1：[parse_quoted_or_token] 带反斜杠转义的引号字符串正确解析
    /// 条件：输入 "\"file\\\"name.txt\""
    /// 断言：返回 "file\"name.txt"
    #[test]
    fn quoted_with_escape() {
        assert_eq!(
            parse_quoted_or_token("\"file\\\"name.txt\""),
            "file\"name.txt"
        );
    }

    /// P1：[parse_quoted_or_token] 带双反斜杠的引号字符串正确解析为单个反斜杠
    /// 条件：输入 "\"a\\\\b\""
    /// 断言：返回 "a\\b"
    #[test]
    fn quoted_with_backslash_escape() {
        assert_eq!(parse_quoted_or_token("\"a\\\\b\""), "a\\b");
    }

    /// P0：[parse_quoted_or_token] 无引号的 token 原样返回
    /// 条件：输入 "report.pdf"
    /// 断言：返回 "report.pdf"
    #[test]
    fn token_simple() {
        assert_eq!(parse_quoted_or_token("report.pdf"), "report.pdf");
    }

    /// P1：[parse_quoted_or_token] token 在分号处截断
    /// 条件：输入 "name.txt; extra"
    /// 断言：返回 "name.txt"
    #[test]
    fn token_stops_at_semicolon() {
        assert_eq!(parse_quoted_or_token("name.txt; extra"), "name.txt");
    }

    /// P1：[parse_quoted_or_token] token 在空白字符处截断
    /// 条件：输入 "name.txt other"
    /// 断言：返回 "name.txt"
    #[test]
    fn token_stops_at_whitespace() {
        assert_eq!(parse_quoted_or_token("name.txt other"), "name.txt");
    }

    /// P1：[parse_quoted_or_token] 空引号字符串返回空串
    /// 条件：输入 "\"\""
    /// 断言：返回 ""
    #[test]
    fn empty_quoted_string() {
        assert_eq!(parse_quoted_or_token("\"\""), "");
    }

    /// P0：[percent_decode] 无编码的 ASCII 字符原样输出
    /// 条件：输入 "hello"
    /// 断言：percent_decode 返回 "hello"
    #[test]
    fn decode_ascii() {
        assert_eq!(percent_decode("hello").unwrap(), "hello");
    }

    /// P1：[percent_decode] URL 编码的大写字母正确解码
    /// 条件：输入 "%48%65%6C%6C%6F"
    /// 断言：percent_decode 返回 "Hello"
    #[test]
    fn decode_encoded_ascii() {
        assert_eq!(percent_decode("%48%65%6C%6C%6F").unwrap(), "Hello");
    }

    /// P1：[percent_decode] 中文 UTF-8 的 URL 编码正确解码
    /// 条件：输入 "%E4%B8%AD"（"中" 的 UTF-8 URL 编码）
    /// 断言：percent_decode 返回 "中"
    #[test]
    fn decode_chinese_utf8() {
        assert_eq!(percent_decode("%E4%B8%AD").unwrap(), "中");
    }

    /// P1：[percent_decode] 混合编码与普通字符正确组合
    /// 条件：输入 "file%20name.txt"（%20 为空格）
    /// 断言：percent_decode 返回 "file name.txt"
    #[test]
    fn decode_mixed() {
        assert_eq!(percent_decode("file%20name.txt").unwrap(), "file name.txt");
    }

    /// P1：[percent_decode] 无效十六进制百分号编码返回 None
    /// 条件：输入 "%ZZ"（非合法 hex）
    /// 断言：percent_decode 返回 None
    #[test]
    fn decode_invalid_hex_returns_none() {
        assert!(percent_decode("%ZZ").is_none());
    }

    /// P1：[percent_decode] 截断的百分号（仅一位 hex）返回 None
    /// 条件：输入 "%4"
    /// 断言：percent_decode 返回 None
    #[test]
    fn decode_truncated_percent_returns_none() {
        assert!(percent_decode("%4").is_none());
    }

    /// P1：[percent_decode] 尾部孤立的百分号返回 None
    /// 条件：输入 "hello%"（百分号后无十六进制位）
    /// 断言：percent_decode 返回 None
    #[test]
    fn decode_trailing_percent_returns_none() {
        assert!(percent_decode("hello%").is_none());
    }

    /// P0：[parse_ext_value] 基本 UTF-8 ext-value 解析
    /// 条件：输入 "UTF-8''hello%20world"
    /// 断言：返回 "hello world"
    #[test]
    fn ext_value_utf8() {
        assert_eq!(
            parse_ext_value("UTF-8''hello%20world").unwrap(),
            "hello world"
        );
    }

    /// P1：[parse_ext_value] 带语言标签的 UTF-8 ext-value 能正确解析
    /// 条件：输入 "UTF-8'en'report.pdf"
    /// 断言：parse_ext_value 返回 "report.pdf"
    #[test]
    fn ext_value_utf8_with_language() {
        assert_eq!(
            parse_ext_value("UTF-8'en'report.pdf").unwrap(),
            "report.pdf"
        );
    }

    /// P1：[parse_ext_value] ext-value 的 UTF-8 大小写不敏感
    /// 条件：输入 "utf-8''test.txt"（小写 utf-8）
    /// 断言：parse_ext_value 返回 "test.txt"
    #[test]
    fn ext_value_utf8_case_insensitive() {
        assert_eq!(parse_ext_value("utf-8''test.txt").unwrap(), "test.txt");
    }

    /// P1：[parse_ext_value] 非 UTF-8 字符集的 ext-value 返回 None
    /// 条件：输入 "ISO-8859-1''test.txt"
    /// 断言：parse_ext_value 返回 None
    #[test]
    fn ext_value_non_utf8_returns_none() {
        assert!(parse_ext_value("ISO-8859-1''test.txt").is_none());
    }

    /// P1：[parse_ext_value] 缺少必要部分的 ext-value 返回 None
    /// 条件：输入 "UTF-8"（缺少语言标签和编码值）
    /// 断言：parse_ext_value 返回 None
    #[test]
    fn ext_value_missing_parts_returns_none() {
        assert!(parse_ext_value("UTF-8").is_none());
    }

    /// P1：[parse_ext_value] 中文文件名的 ext-value 正确解码
    /// 条件：输入 "UTF-8''%E6%96%87%E4%BB%B6.pdf"
    /// 断言：parse_ext_value 返回 "文件.pdf"
    #[test]
    fn ext_value_chinese_filename() {
        assert_eq!(
            parse_ext_value("UTF-8''%E6%96%87%E4%BB%B6.pdf").unwrap(),
            "文件.pdf"
        );
    }
}
