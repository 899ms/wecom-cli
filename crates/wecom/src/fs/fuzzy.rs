//! Path fuzzy-correction primitives: Levenshtein distance, per-level `pick`,
//! and in-memory root-trie anchoring.  These are pure functions that never touch
//! the filesystem — suitable for exhaustive testing with sanitised strings.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use strsim::levenshtein;

use crate::{Error, Result};

// ── Constants ──────────────────────────────────────────────

/// Per-level relevance cap (normalised): champion edit distance must be
/// ≤ floor(len_ref × R_MAX) when the correction is not exact.
const LEVEL_R_MAX: f64 = 0.34;

/// Maximum absolute edit distance per segment.  Even long names
/// (where the percentage allows more) are capped here — an AI typo
/// is typically 1–2 chars, and ≥ 4 edits means the segment is
/// semantically different (e.g. `apple.md` vs `banana.md`).
const LEVEL_MAX_DIST: usize = 3;

/// Per-level relative margin (integer comparison: d2·2 ≥ d1·3 ⇔ d2 ≥ 1.5·d1).
const LEVEL_MARGIN_RATIO_NUM: usize = 3;
const LEVEL_MARGIN_RATIO_DEN: usize = 2;

/// Per-level absolute margin: the runner-up must be at least this many edits
/// worse than the champion.  Guards against thin-margin *typo* corrections
/// (e.g. `cat` → `car` when `cars` also exists, dd1=1/dd2=2).  Pure
/// whitespace / case noise (`data(1).md` → `data (1).md`, `File.TXT` →
/// `file.txt`) is resolved earlier by the noise-insensitive tier, so this gate
/// no longer blocks those cases.
const LEVEL_MARGIN_ABS: usize = 2;

/// Max fan-out per filesystem level during phase 2; exceeding this
/// aborts correction (prevents pathological large directories).
pub(crate) const LEVEL_FANOUT_MAX: usize = 10_000;

/// Max tail depth during phase 2 (prevents deep-path blow-up).
pub(crate) const WALK_DEPTH_MAX: usize = 16;

/// Normalise a segment for the noise-insensitive match tier: remove all
/// Unicode whitespace **and** fold to lowercase.  Missing / extra spaces and
/// case differences are the dominant noise classes (AI drops or inserts
/// spaces and mis-cases characters; some filesystems are case-insensitive),
/// so two segments sharing the same normalised form are treated as the same
/// segment modulo spacing and case.
fn normalize_noise(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

// ── Unified per-level pick ─────────────────────────────────

/// Result of a single-level `pick` decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LevelPick {
    /// Exact match (dd1 == 0).
    Exact(String),
    /// High-confidence correction (dd1 > 0, passed both gates).
    Corrected(String),
    /// Low confidence / tie / empty candidates → abort.
    Abort,
}

/// Per-level decision primitive shared by both phases.
///
/// `seg` is the input segment; `candidates` are the real names at this
/// level (root-trie children for phase 1, filesystem entries for phase 2).
///
/// Decision tiers (first match wins):
/// 1. **Exact** — a candidate equals `seg` verbatim.  Not a guess: accept.
/// 2. **Noise-insensitive unique match** — exactly one candidate equals `seg`
///    once whitespace is removed and case is folded.  The only differences are
///    spacing / case (the dominant noise classes), a high-confidence
///    correction independent of the fuzzy margin.  This is what lets
///    `data(1).md` resolve to `data (1).md` (and `File.TXT` to `file.txt`)
///    even though a sibling sits only one extra edit away.  Two or more
///    candidates collapsing to the same normalised form are ambiguous →
///    `Abort`.
/// 3. **Fuzzy Levenshtein** — champion must pass the relevance gate and beat
///    the runner-up by both an absolute and a relative margin.  Guards typo
///    corrections; ties and thin margins `Abort`.
pub(crate) fn pick(seg: &str, candidates: &[String]) -> LevelPick {
    if candidates.is_empty() {
        return LevelPick::Abort;
    }

    // Tier 1: exact hit — not a guess, accept unconditionally.
    if let Some(c) = candidates.iter().find(|c| c.as_str() == seg) {
        return LevelPick::Exact(c.clone());
    }

    // Tier 2: noise-insensitive unique match (whitespace + case folding).
    let seg_key = normalize_noise(seg);
    let mut noise_hit: Option<&str> = None;
    let mut noise_hits = 0usize;
    for c in candidates {
        if normalize_noise(c) == seg_key {
            noise_hits += 1;
            noise_hit = Some(c.as_str());
        }
    }
    match noise_hits {
        // Exactly one candidate matches modulo whitespace/case → high confidence.
        1 => return LevelPick::Corrected(noise_hit.expect("noise_hits == 1 ⇒ Some").to_string()),
        // Ambiguous: cannot tell which the user meant → refuse to guess.
        n if n >= 2 => return LevelPick::Abort,
        _ => {}
    }

    // Tier 3: fuzzy Levenshtein.  Find the two smallest distances.
    let mut best: (usize, &str) = (usize::MAX, "");
    let mut second_best: usize = usize::MAX;

    for c in candidates {
        let d = levenshtein(seg, c);
        if d < best.0 {
            second_best = best.0;
            best = (d, c.as_str());
        } else if d < second_best {
            second_best = d;
        }
    }

    let (dd1, c1) = best;
    let dd2 = second_best;

    // dd1 == 0 is impossible here: an exact match would have returned in tier 1.

    // Gate: relevance — is the champion "close enough"?
    let len_ref = seg.chars().count().max(c1.chars().count());
    // For very short segments (≤ 3 chars) a single-char typo is still
    // "close enough"; the percentage threshold would rule it out too
    // aggressively.  Use max(len_ref·R_MAX, 1) as the floor.
    let r_cap = ((len_ref as f64 * LEVEL_R_MAX).floor() as usize).clamp(1, LEVEL_MAX_DIST);
    if dd1 > r_cap {
        return LevelPick::Abort;
    }

    // Gate: margin — champion must be strictly better than runner-up, and the
    // gap must satisfy both the absolute (≥ LEVEL_MARGIN_ABS) and relative
    // (≥ 1.5×) thresholds.  Ties and thin margins are rejected.
    if dd2 != usize::MAX {
        // Tie / worse, or absolute margin too small → reject.
        if dd2 <= dd1 || dd2 - dd1 < LEVEL_MARGIN_ABS {
            return LevelPick::Abort;
        }
        // Integer comparison for 1.5×: dd2 * 2 >= dd1 * 3.
        if dd2 * LEVEL_MARGIN_RATIO_DEN < dd1 * LEVEL_MARGIN_RATIO_NUM {
            return LevelPick::Abort;
        }
    }

    LevelPick::Corrected(c1.to_string())
}

// ── Root Trie (phase 1 — no fs access) ─────────────────────

/// An in-memory trie built from `readable_dirs` segment sequences.
///
/// Each level stores deduplicated child names and marks nodes that
/// correspond to a complete sandbox root.  Phase 1 walks this trie
/// with the input segments using `pick`, never touching the filesystem.
pub(crate) struct RootTrie {
    root: TrieNode,
}

#[derive(Debug, Clone)]
struct TrieNode {
    children: HashMap<String, TrieNode>,
    /// This node represents a complete readable root (i.e. the end of
    /// one of the original `readable_dirs` paths).
    is_complete_root: bool,
    /// `true` when this node's key is a structural path component
    /// (`Component::RootDir` / `Component::Prefix`), not a directory name.
    /// Structural nodes must never be fuzzy-matched.
    is_structural: bool,
    /// The canonical real path for this node (only meaningful when
    /// `is_complete_root` is true).
    real_path: Option<PathBuf>,
}

impl RootTrie {
    /// Build a root-trie from the given list of readable sandbox root
    /// directories.
    ///
    /// Paths that are not absolute or have empty segment sequences are
    /// silently skipped.
    pub fn build(roots: &[PathBuf]) -> Self {
        let mut trie = Self {
            root: TrieNode {
                children: HashMap::new(),
                is_complete_root: false,
                is_structural: false,
                real_path: None,
            },
        };

        for root in roots {
            // Collect (segment, is_structural) pairs from the path components.
            let comps: Vec<(String, bool)> = root
                .components()
                .map(|c| {
                    let s = c.as_os_str().to_string_lossy().into_owned();
                    let structural = !matches!(c, Component::Normal(_));
                    (s, structural)
                })
                .collect();
            if comps.is_empty() {
                continue;
            }
            let mut node = &mut trie.root;
            for (seg, structural) in &comps {
                node = node
                    .children
                    .entry(seg.clone())
                    .or_insert_with(|| TrieNode {
                        children: HashMap::new(),
                        is_complete_root: false,
                        is_structural: *structural,
                        real_path: None,
                    });
            }
            node.is_complete_root = true;
            node.real_path = Some(root.clone());
        }

        trie
    }

    /// Walk the trie with `input_comps`, returning the anchored real root
    /// directory and the tail segments if a complete root was reached with
    /// sufficient confidence at each level.
    ///
    /// Each input `Component` is matched against the trie:
    /// - **Structural components** (`RootDir`, `Prefix`) must match **exactly**
    ///   — they are path structure, not directory names, and must never be
    ///   fuzzy-corrected.
    /// - **Normal components** use normal `pick` matching.
    ///
    /// `CurDir` / `ParentDir` components are treated as structural (exact match
    /// only) to prevent semantic-breaking corrections.
    ///
    /// Returns `None` if no complete root could be anchored.  Tail length /
    /// emptiness is **not** checked here — the caller decides whether the
    /// tail is acceptable.
    pub fn anchor(&self, input_comps: &[Component<'_>]) -> Option<(PathBuf, Vec<String>)> {
        if input_comps.is_empty() {
            return None;
        }

        let mut node = &self.root;
        let mut i = 0usize;
        // (consumed segment count at the deepest complete root)
        let mut anchor: Option<(usize, &PathBuf)> = None;

        loop {
            if node.is_complete_root {
                anchor = Some((i, node.real_path.as_ref()?));
            }
            if i >= input_comps.len() {
                break;
            }

            let comp = &input_comps[i];
            let seg = comp.as_os_str().to_string_lossy();
            let is_normal = matches!(comp, Component::Normal(_));

            // Build the candidate list for this level.  When the input
            // component is Normal, exclude structural trie children
            // (RootDir / Prefix) so a directory name can never fuzzy-match
            // a path separator (e.g. "a" → "/").
            let child_names: Vec<String> = node
                .children
                .iter()
                .filter(|(_, child)| is_normal != child.is_structural)
                .map(|(k, _)| k.clone())
                .collect();
            if child_names.is_empty() {
                break;
            }

            let matched_name = if is_normal {
                match pick(&seg, &child_names) {
                    LevelPick::Exact(name) | LevelPick::Corrected(name) => name,
                    LevelPick::Abort => break,
                }
            } else {
                // Exact match only for structural components.
                child_names.into_iter().find(|c| c == seg.as_ref())?
            };

            node = node.children.get(&matched_name)?;
            i += 1;
        }

        let (k, real_path) = anchor?;
        let tail: Vec<String> = input_comps[k..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some((real_path.clone(), tail))
    }
}

// ── Phase 2: per-level filesystem walk ────────────────────

/// Phase 2: per-level fuzzy Tab-walk on real filesystem entries.
///
/// Walks `tail` segments starting from `anchor`, listing each level's
/// entries via [`super::sandbox::sandbox_list_dir`] and matching with
/// [`pick`].  Returns the resolved path on success, or a uniform
/// "file not found" error on any per-level failure (empty candidates,
/// fan-out exceeded, or a level that failed confidence gates).
///
/// `roots` is forwarded to `sandbox_list_dir` for sandbox validation on
/// each listing.  `file_path` is the original user-supplied path, used
/// only to build the error message.
pub(crate) fn walk_tail(
    anchor: PathBuf,
    tail: &[String],
    roots: Option<&[PathBuf]>,
    file_path: &str,
) -> Result<PathBuf> {
    let mut cur = anchor;
    for (j, seg) in tail.iter().enumerate() {
        let is_final = j == tail.len() - 1;
        let entries = super::sandbox::sandbox_list_dir(&cur, roots)?;
        let candidates: Vec<PathBuf> = entries
            .into_iter()
            .filter(|p| if is_final { p.is_file() } else { p.is_dir() })
            .collect();

        if candidates.is_empty() || candidates.len() > LEVEL_FANOUT_MAX {
            return Err(readable_not_found(file_path));
        }

        let candidate_names: Vec<String> = candidates
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();

        match pick(seg, &candidate_names) {
            LevelPick::Exact(name) | LevelPick::Corrected(name) => {
                // Use the matched candidate path directly instead of
                // re-joining the name to `cur`.
                cur = candidates
                    .into_iter()
                    .find(|p| p.file_name().is_some_and(|n| n == name.as_str()))
                    .unwrap_or_else(|| cur.join(&name));
            }
            LevelPick::Abort => {
                return Err(readable_not_found(file_path));
            }
        }
    }
    Ok(cur)
}

/// Uniform "target file not found" error for correction failures.
///
/// Used only when anchoring succeeded but a per-level confidence gate
/// failed (file genuinely not found).  Anchor failures surface the
/// original `check_readable` error instead — see
/// [`super::Fs::resolve_readable_or_suggest`].
pub(crate) fn readable_not_found(file_path: &str) -> Error {
    Error::Other(format!("找不到目标文件: {file_path}").into())
}

/// Find the deepest existing ancestor directory of `path`, returning
/// `(ancestor, tail_segments)` where `tail_segments` are the non-existent
/// components below the ancestor.
///
/// For an absolute path like `/a/b/c/d`, if `/a/b` exists but `/a/b/c`
/// does not, returns `(PathBuf::from("/a/b"), vec!["c".into(), "d".into()])`.
///
/// Returns an error when `path` itself is an existing directory (no tail
/// to correct).
pub(crate) fn deepest_existing_ancestor(path: &Path) -> Option<(PathBuf, Vec<String>)> {
    let comps: Vec<_> = path.components().collect();

    // Walk prefixes from deepest (full path) toward root, stopping at the
    // first existing directory.  Start at 1 to skip the degenerate empty
    // prefix when path has no root component.
    for i in (1..=comps.len()).rev() {
        let prefix: PathBuf = comps[..i].iter().collect();
        if prefix.exists() && prefix.is_dir() {
            let tail: Vec<_> = comps[i..]
                .iter()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            return if tail.is_empty() {
                None
            } else {
                Some((prefix, tail))
            };
        }
    }

    None
}

// ══════════════════════════════════════════════════════════════
//  Tests — lev, pick, RootTrie (pure, no fs)
// ══════════════════════════════════════════════════════════════

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：fuzzy（路径模糊纠错纯逻辑 — levenshtein + pick + RootTrie + walk_tail）
    //!
    //! ### 关键接口
    //! - [pick] — 统一逐级决策原语（阶段一/二共用）
    //! - [RootTrie::build] / [RootTrie::anchor] — 内存 root-trie 锚定（阶段一）
    //! - [walk_tail] — 阶段二 fs 逐级下潜（同步，委托 sandbox_list_dir）
    //! - [deepest_existing_ancestor] — unrestricted 模式取最深存在祖先
    //! - [readable_not_found] — 纠错失败的统一错误构造
    //!
    //! ### 关键分支与异常路径
    //! - 精确命中（seg == 候选）→ Exact（不经后续闸门）
    //! - 噪声不敏感唯一匹配（去空格 + 折叠大小写后内容相同且唯一）→ Corrected（不依赖裕度）
    //! - 噪声不敏感多候选歧义 → Abort（无法判断，绝不误匹配）
    //! - 高置信纠正 → Corrected（相关性 + 绝对/相对裕度三闸门通过）
    //! - 平局 / 薄裕度（dd2−dd1 < 2 或比值 < 1.5×）→ Abort（绝不误匹配）
    //! - 空候选 / 唯一候选但相关性不足 → Abort
    //! - RootTrie 嵌套 root（前缀 root）→ 取最深可信命中
    //! - RootTrie 无任何完整 root 可达 → None
    //! - walk_tail 任一级 Abort / 空候选 / 超扇出 → readable_not_found
    //! - deepest_existing_ancestor 路径完全存在 → Err（无 tail 可纠）
    //!
    //! ### 上下游交互
    //! - 上游：[fs::mod] 的 [Fs::resolve_readable_or_suggest]（async 编排）
    //! - 下游：walk_tail 委托 [super::sandbox::sandbox_list_dir]（fs 读取）

    use super::*;

    // ── strsim::levenshtein (char-level contract) ──

    /// P0：strsim::levenshtein 完全相同的字符串距离为 0
    /// 条件：输入 "abc" 与 "abc"
    /// 断言：距离 == 0
    #[test]
    fn levenshtein_identical_zero() {
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    /// P0：strsim::levenshtein 空格插入使段内距离为 1
    /// 条件："workspace" vs "work space"（多 1 空格）
    /// 断言：距离 == 1
    #[test]
    fn levenshtein_one_space_insertion() {
        assert_eq!(levenshtein("workspace", "work space"), 1);
    }

    /// P1：strsim::levenshtein 两字符完全不同距离为较长者长度
    /// 条件："abc" vs "xyz"
    /// 断言：距离 == 3
    #[test]
    fn levenshtein_fully_different() {
        assert_eq!(levenshtein("abc", "xyz"), 3);
    }

    /// P1：strsim::levenshtein 空字符串距离为对方长度
    /// 条件："" vs "hello"
    /// 断言：距离 == 5
    #[test]
    fn levenshtein_empty_string() {
        assert_eq!(levenshtein("", "hello"), 5);
        assert_eq!(levenshtein("hello", ""), 5);
    }

    /// P2：strsim::levenshtein 含中文日文字符按 char 计距
    /// 条件："文件" vs "文档"（前一个字不同）
    /// 断言：距离 == 1（仅 1 次替换）
    #[test]
    fn levenshtein_cjk_chars() {
        assert_eq!(levenshtein("文件", "文档"), 1);
    }

    /// P1：strsim::levenshtein 单字符删除正确计距
    /// 条件："abc" vs "ab"（尾部多一个字符 -> 删除）
    /// 断言：距离 == 1
    #[test]
    fn levenshtein_one_deletion() {
        assert_eq!(levenshtein("abc", "ab"), 1);
    }

    /// P1：strsim::levenshtein 单字符替换正确计距
    /// 条件："cat" vs "bat"（1 替换）
    /// 断言：距离 == 1
    #[test]
    fn levenshtein_one_substitution() {
        assert_eq!(levenshtein("cat", "bat"), 1);
    }

    /// P2：strsim::levenshtein 大小写差异按替换计距（不做 case folding）
    /// 条件："file" vs "File"（首字符不同 case）
    /// 断言：距离 == 1
    #[test]
    fn levenshtein_case_difference() {
        assert_eq!(levenshtein("file", "File"), 1);
    }

    // ── pick ──

    /// P0：[pick] 精确命中优先，不被兄弟裕度误伤
    /// 条件：seg="data (1).md"，candidates=["data (1).md","data (2).md"]
    /// 断言：Exact("data (1).md")
    #[test]
    fn pick_exact_with_near_sibling() {
        let result = pick("data (1).md", &["data (1).md".into(), "data (2).md".into()]);
        assert_eq!(result, LevelPick::Exact("data (1).md".into()));
    }

    /// P0：[pick] 高区分度纠正——冠军 dd1=1，亚军 dd2≈9
    /// 条件：seg="abcd efgh", candidates=["abcdefgh","jaij39rj39"]
    /// 断言：Corrected("abcdefgh")
    #[test]
    fn pick_high_margin_correction() {
        let result = pick("abcd efgh", &["abcdefgh".into(), "jaij39rj39".into()]);
        assert_eq!(result, LevelPick::Corrected("abcdefgh".into()));
    }

    /// P0：[pick] 空格不敏感唯一匹配 —— 去空格后与唯一候选内容相同
    /// 条件：seg="data(1).md", candidates=["data (1).md","data (2).md"]
    /// 断言：Corrected("data (1).md")（去空格后 data(1).md 仅命中 (1)，序号 2 不匹配）
    #[test]
    fn pick_whitespace_insensitive_unique_match() {
        let result = pick("data(1).md", &["data (1).md".into(), "data (2).md".into()]);
        assert_eq!(result, LevelPick::Corrected("data (1).md".into()));
    }

    /// P0：[pick] 空格不敏感匹配反向安全 —— 输入 (2) 只命中 (2)
    /// 条件：seg="data(2).md", candidates=["data (1).md","data (2).md"]
    /// 断言：Corrected("data (2).md")（去空格后仅命中 (2)，绝不纠成 (1)）
    #[test]
    fn pick_whitespace_insensitive_reverse_safe() {
        let result = pick("data(2).md", &["data (1).md".into(), "data (2).md".into()]);
        assert_eq!(result, LevelPick::Corrected("data (2).md".into()));
    }

    /// P1：[pick] 空格不敏感匹配歧义 —— 多候选去空格后都等于输入
    /// 条件：seg="a bc", candidates=["abc","ab c"]（两者去空格均为 "abc"）
    /// 断言：Abort（无法判断用户所指，绝不误匹配）
    #[test]
    fn pick_whitespace_insensitive_ambiguous_aborts() {
        let result = pick("a bc", &["abc".into(), "ab c".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] ID 段整体去空格唯一命中（脱敏自线上 ID 噪声）
    /// 条件：seg="ZPog D6Q", candidates=["ZPogD6Q","otherlongname"]
    /// 断言：Corrected("ZPogD6Q")（去空格后内容一致，与另一候选无歧义）
    #[test]
    fn pick_whitespace_insensitive_id_segment() {
        let result = pick("ZPog D6Q", &["ZPogD6Q".into(), "otherlongname".into()]);
        assert_eq!(result, LevelPick::Corrected("ZPogD6Q".into()));
    }

    /// P0：[pick] 大小写不敏感唯一匹配 —— 仅大小写差异
    /// 条件：seg="File.TXT", candidates=["file.txt","other.txt"]
    /// 断言：Corrected("file.txt")（折叠大小写后唯一命中，不要求裕度）
    #[test]
    fn pick_case_insensitive_unique_match() {
        let result = pick("File.TXT", &["file.txt".into(), "other.txt".into()]);
        assert_eq!(result, LevelPick::Corrected("file.txt".into()));
    }

    /// P1：[pick] 空格 + 大小写混合噪声唯一匹配
    /// 条件：seg="Data (1).MD", candidates=["data(1).md","data(2).md"]
    /// 断言：Corrected("data(1).md")（去空格 + 折叠大小写后唯一命中 (1)）
    #[test]
    fn pick_noise_insensitive_space_and_case() {
        let result = pick("Data (1).MD", &["data(1).md".into(), "data(2).md".into()]);
        assert_eq!(result, LevelPick::Corrected("data(1).md".into()));
    }

    /// P1：[pick] 大小写不敏感匹配歧义 —— 多候选仅大小写不同
    /// 条件：seg="FILE.txt", candidates=["file.txt","File.txt"]
    /// 断言：Abort（折叠大小写后两候选均等于输入，无法判断，绝不误匹配）
    #[test]
    fn pick_case_insensitive_ambiguous_aborts() {
        let result = pick("FILE.txt", &["file.txt".into(), "File.txt".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 完全平局拒绝（dd1 == dd2）
    /// 条件：seg="x", candidates=["xa","xb"]（两个距离都是 1）
    /// 断言：Abort
    #[test]
    fn pick_exact_tie_aborts() {
        let result = pick("x", &["xa".into(), "xb".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 唯一条目、可纠正
    /// 条件：seg="abcd efgh", candidates=["abcdefgh"]
    /// 断言：Corrected("abcdefgh")
    #[test]
    fn pick_single_candidate_correctable() {
        let result = pick("abcd efgh", &["abcdefgh".into()]);
        assert_eq!(result, LevelPick::Corrected("abcdefgh".into()));
    }

    /// P1：[pick] 唯一条目、相关性不足
    /// 条件：seg="abc", candidates=["totally_different_long"]
    /// 断言：Abort（d1 过大）
    #[test]
    fn pick_single_candidate_relevance_fails() {
        let result = pick("abc", &["totally_different_long".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 空候选集
    /// 条件：seg="anything", candidates=[]
    /// 断言：Abort
    #[test]
    fn pick_empty_candidates_aborts() {
        let result = pick("anything", &[]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 绝对编辑距离超限（dd1 > LEVEL_MAX_DIST）
    /// 条件：seg="strawberry", candidates=["blueberry"]（dist ≈ 4）
    /// 断言：Abort（虽然百分比可能过，但绝对距离 > 3 必须拒绝）
    #[test]
    fn pick_absolute_dist_cap_aborts() {
        // "strawberry" (10) → "blueberry" (9): straw→blue = 4+ edits
        let result = pick("strawberry", &["blueberry".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P2：[pick] 相对裕度不足（dd2 够大但比值不满足 1.5×）
    /// 条件：构造 dd1=10, dd2=12，比值 12/10=1.2<1.5
    /// 断言：Abort（dd1=10 > LEVEL_MAX_DIST 也会触发，但差距足够大）
    #[test]
    fn pick_margin_ratio_fails() {
        let result = pick(
            "aaaaaaaaaa",                                  // 10 a's
            &["bbbbbbbbbb".into(), "cccccccccccc".into()], // dist ≈ 10 vs 12
        );
        assert_eq!(result, LevelPick::Abort);
    }

    /// P2：[pick] 极短段落单字符笔误通过 r_cap=max(floor(len*0.34),1) 兜底
    /// 条件：seg="ab"，candidate="ac"（距离 1，r_cap=max(0,1)=1）
    /// 断言：Corrected("ac")
    #[test]
    fn pick_short_seg_single_typo_corrected() {
        let result = pick("ab", &["ac".into()]);
        assert_eq!(result, LevelPick::Corrected("ac".into()));
    }

    /// P2：[pick] 极短段落相关性失败（距离超过 r_cap 兜底值）
    /// 条件：seg="ab"，candidate="cd"（距离 2，r_cap=max(0,1)=1，2>1）
    /// 断言：Abort
    #[test]
    fn pick_short_seg_relevance_fails() {
        let result = pick("ab", &["cd".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P2：[pick] 重复非精确候选导致平局
    /// 条件：candidates 含两个同名条目 "data (1).md"，非精确匹配时 dd1=dd2
    /// 断言：Abort
    #[test]
    fn pick_duplicate_non_exact_candidates_tie() {
        let result = pick("data.md", &["data (1).md".into(), "data (1).md".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P2：[pick] 精确匹配单候选
    /// 条件：seg="hello"，candidates=["hello"]
    /// 断言：Exact("hello")
    #[test]
    fn pick_exact_single_candidate() {
        let result = pick("hello", &["hello".into()]);
        assert_eq!(result, LevelPick::Exact("hello".into()));
    }

    /// P2：[pick] 精确匹配多候选中有重复
    /// 条件：candidates 包含精确匹配和另一个同样距离的候选（两个完全相同）
    /// 断言：Exact（不因 dd2=dd1 而中断）
    #[test]
    fn pick_exact_with_duplicate_exact_candidates() {
        let result = pick("foo", &["foo".into(), "foo".into()]);
        assert_eq!(result, LevelPick::Exact("foo".into()));
    }

    /// P1：[pick] dd1 恰好在 LEVEL_MAX_DIST 边界，相关性通过
    /// 条件：seg 与唯一候选距离 == 3，len_ref 使得 r_cap==3
    /// 断言：Corrected
    #[test]
    fn pick_dd1_exactly_max_dist_boundary() {
        // "aaaaaaaaa" (9) → "aaabbbaaa" (9): change 3 a's to b's = 3
        // len_ref=9, floor(9*0.34)=3, clamp(3,1,3)=3, dd1=3 ≤ 3 ✓
        let result = pick("aaaaaaaaa", &["aaabbbaaa".into()]);
        assert_eq!(result, LevelPick::Corrected("aaabbbaaa".into()));
    }

    /// P1：[pick] dd1 > LEVEL_MAX_DIST，唯一候选相关性失败
    /// 条件：seg 与唯一候选距离 == 4 > 3
    /// 断言：Abort
    #[test]
    fn pick_dd1_4_single_candidate_aborts() {
        // "aaaaaaaaaa" (10) → "aaaabbbbaa" (10): change 4 a's → b's = 4 > 3
        let result = pick("aaaaaaaaaa", &["aaaabbbbaa".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 相对比值达标但绝对裕度不足（dd1=2, dd2=3）
    /// 条件：dd1=2, dd2=3，比值 3·2=6 ≥ 2·3=6 达标，但 dd2−dd1=1 < MARGIN_ABS(2)
    /// 断言：Abort（薄裕度笔误绝不纠错，兑现"绝不误匹配"）
    #[test]
    fn pick_ratio_ok_but_abs_margin_fails() {
        // "aaaaaa" → "aabbaa" (2 edits: change a→b at pos 2,3)
        // "aaaaaa" → "aabbca" (3 edits: change a→b at pos 2,3 + a→c at pos 4)
        // len_ref=6, floor(6*0.34)=2, cap=2, dd1=2 ≤ 2 ✓
        // dd2=3 > dd1 ✓, 但 dd2−dd1=1 < 2 ✗ → Abort
        let result = pick("aaaaaa", &["aabbaa".into(), "aabbca".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] dd1=3 dd2=4 绝对裕度不足（4−3=1 < 2）
    /// 条件：dd1=3 通过相关性与绝对值上限，但冠亚军仅差 1
    /// 断言：Abort（绝对裕度闸门先于比值闸门拒绝）
    #[test]
    fn pick_dd1_3_dd2_4_margin_fails() {
        // "aaaaaaaaa" (9) → "aaabbbaaa" (3 edits)
        // "aaaaaaaaa" (9) → "aaabbbbaa" (4 edits)
        // len_ref=9, floor(9*0.34)=3, cap=3, dd1=3 ≤ 3 ✓
        // dd2=4 > dd1 ✓, 但 dd2−dd1=1 < 2 ✗ → Abort
        let result = pick("aaaaaaaaa", &["aaabbbaaa".into(), "aaabbbbaa".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P1：[pick] 空输入段匹配单字符候选（dd1=1, r_cap=1）
    /// 条件：seg=""，candidates=["a"]，距离 1，r_cap=clamp(floor(0),1,3)=1
    /// 断言：Corrected("a")
    #[test]
    fn pick_empty_seg_single_candidate() {
        let result = pick("", &["a".into()]);
        assert_eq!(result, LevelPick::Corrected("a".into()));
    }

    /// P2：[pick] 空输入段距离超限（dd1>r_cap=1）导致 Abort
    /// 条件：seg=""，candidates=["ab"]，距离 2 > r_cap=1
    /// 断言：Abort
    #[test]
    fn pick_empty_seg_distance_exceeds_cap() {
        let result = pick("", &["ab".into()]);
        assert_eq!(result, LevelPick::Abort);
    }

    /// P2：[pick] 空输入段精确匹配空候选
    /// 条件：seg=""，candidates=[""]
    /// 断言：Exact("")
    #[test]
    fn pick_empty_seg_exact_match_empty_candidate() {
        let result = pick("", &["".into()]);
        assert_eq!(result, LevelPick::Exact("".into()));
    }

    // ── RootTrie ──

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    /// Build `Vec<Component>` from a path string, mimicking how the real
    /// caller (`Fs::resolve_readable_or_suggest`) produces components via
    /// `Path::components()`.  This preserves component type information so
    /// `anchor` can distinguish `RootDir` from `Normal`.
    fn comps(p: &str) -> Vec<Component<'_>> {
        Path::new(p).components().collect()
    }

    /// P0：[RootTrie::anchor] root 名空格、多 sibling root
    /// 条件：input=["/","work space","data1.md"], roots=["/skills","/data","/workspace"]
    /// 断言：Some{root=/workspace, tail=[data1.md]}
    #[test]
    fn trie_root_name_space_multi_sibling() {
        let trie = RootTrie::build(&paths(&["/skills", "/data", "/workspace"]));
        let result = trie.anchor(&comps("/work space/data1.md"));
        let (root, tail) = result.expect("should anchor to workspace");
        assert_eq!(root, PathBuf::from("/workspace"));
        assert_eq!(tail, vec!["data1.md".to_string()]);
    }

    /// P0：[RootTrie::anchor] 深 root 段空格
    /// 条件：input=["/","a","b ","c","x.md"], root=["/a/b/c"]
    /// 断言：Some{root=/a/b/c, tail=[x.md]}
    #[test]
    fn trie_deep_root_segment_space() {
        let trie = RootTrie::build(&paths(&["/a/b/c"]));
        let result = trie.anchor(&comps("/a/b /c/x.md"));
        let (root, tail) = result.expect("should anchor to /a/b/c");
        assert_eq!(root, PathBuf::from("/a/b/c"));
        assert_eq!(tail, vec!["x.md".to_string()]);
    }

    /// P1：[RootTrie::anchor] 共享前缀、不同末段——取最深完整 root
    /// 条件：roots=["/a/wecom/corpX/temp/sess1","/a/wecom/otherCorp/temp/sess2"]
    ///        input=["/","a","wecom","corp X","temp","sess1","f.md"]
    /// 断言：Some{root=/a/wecom/corpX/temp/sess1, tail=[f.md]}
    #[test]
    fn trie_shared_prefix_correct_branch() {
        let trie = RootTrie::build(&paths(&[
            "/a/wecom/corpX/temp/sess1",
            "/a/wecom/otherCorp/temp/sess2",
        ]));
        let result = trie.anchor(&comps("/a/wecom/corp X/temp/sess1/f.md"));
        let (root, tail) = result.expect("should anchor to corpX/sess1");
        assert_eq!(root, PathBuf::from("/a/wecom/corpX/temp/sess1"));
        assert_eq!(tail, vec!["f.md".to_string()]);
    }

    /// P1：[RootTrie::anchor] 无接近 root（首层相关性不足）
    /// 条件：input=["/","zzzzz","x.md"], roots=["/skills","/data"]
    /// 断言：None
    #[test]
    fn trie_no_close_root_aborts() {
        let trie = RootTrie::build(&paths(&["/skills", "/data"]));
        assert!(trie.anchor(&comps("/zzzzz/x.md")).is_none());
    }

    /// P1：[RootTrie::anchor] 输入止于 root 本身 → 返回空 tail（由调用方检查）
    /// 条件：input=["/","workspace"], root=["/workspace"]
    /// 断言：Some{root=/workspace, tail=[]}（anchor 找到但 tail 为空）
    #[test]
    fn trie_input_stops_at_root_no_tail() {
        let trie = RootTrie::build(&paths(&["/workspace"]));
        let (root, tail) = trie
            .anchor(&comps("/workspace"))
            .expect("should anchor to /workspace");
        assert_eq!(root, PathBuf::from("/workspace"));
        assert!(tail.is_empty(), "tail should be empty");
    }

    /// P1：[RootTrie::anchor] trie 首层近似平局
    /// 条件：两个 root 名仅差 1 字符，首段 dd 近似相同
    /// 断言：None（区分度不足）
    #[test]
    fn trie_first_level_near_tie_aborts() {
        let trie = RootTrie::build(&paths(&["/rootA/dir", "/rootB/dir"]));
        let result = trie.anchor(&comps("/rootX/file.md"));
        assert!(result.is_none());
    }

    /// P2：[RootTrie::anchor] 嵌套 root（一个 root 是另一个前缀）
    /// 条件：roots=["/a/b","/a/b/c"]，input=["/","a","b","c","f.md"]
    /// 断言：Some{root=/a/b/c, tail=[f.md]}（取最深完整 root）
    #[test]
    fn trie_nested_roots_deepest_wins() {
        let trie = RootTrie::build(&paths(&["/a/b", "/a/b/c"]));
        let result = trie.anchor(&comps("/a/b/c/f.md"));
        let (root, tail) = result.expect("should pick deepest /a/b/c");
        assert_eq!(root, PathBuf::from("/a/b/c"));
        assert_eq!(tail, vec!["f.md".to_string()]);
    }

    /// P1：[RootTrie::anchor] 单 root 精确匹配含 tail
    /// 条件：roots=["/workspace"]，input=["/","workspace","src","main.rs"]
    /// 断言：Some{root=/workspace, tail=[src, main.rs]}
    #[test]
    fn trie_single_root_exact_with_tail() {
        let trie = RootTrie::build(&paths(&["/workspace"]));
        let result = trie.anchor(&comps("/workspace/src/main.rs"));
        let (root, tail) = result.expect("should anchor to /workspace");
        assert_eq!(root, PathBuf::from("/workspace"));
        assert_eq!(tail, vec!["src".to_string(), "main.rs".to_string()]);
    }

    /// P1：[RootTrie::anchor] 空 root 列表返回 None
    /// 条件：roots=[]
    /// 断言：None
    #[test]
    fn trie_empty_roots_returns_none() {
        let trie = RootTrie::build(&[]);
        assert!(trie.anchor(&comps("/anything")).is_none());
    }

    /// P1：[RootTrie::anchor] 部分匹配但未到达任何完整 root
    /// 条件：roots=["/a/b/c/d"]，input=["/","a","b","x.md"]——到达 /a/b 但不是完整 root
    /// 断言：None
    #[test]
    fn trie_partial_match_no_complete_root() {
        let trie = RootTrie::build(&paths(&["/a/b/c/d"]));
        // /a/b exists as a non-complete inner node, no tail past any complete root.
        assert!(trie.anchor(&comps("/a/b/x.md")).is_none());
    }

    /// P1：[RootTrie::anchor] tail 超过 WALK_DEPTH_MAX → 仍返回 Some（由调用方检查长度）
    /// 条件：input 在 root 后有 17 个 tail 段
    /// 断言：Some{root=/root}，tail.len() == 17（> WALK_DEPTH_MAX，调用方拒绝）
    #[test]
    fn trie_tail_exceeds_max_depth() {
        let trie = RootTrie::build(&paths(&["/root"]));
        let mut segs: Vec<String> = vec!["/".into(), "root".into()];
        for i in 0..17 {
            segs.push(format!("level{i}"));
        }
        let (root, tail) = trie
            .anchor(&comps(&segs.join("/")))
            .expect("should anchor to /root");
        assert_eq!(root, PathBuf::from("/root"));
        assert_eq!(tail.len(), 17);
        assert!(tail.len() > WALK_DEPTH_MAX);
    }

    /// P2：[RootTrie::anchor] root 名模糊纠正（1 字符笔误）
    /// 条件：roots=["/data"]，input=["/","dta","f.md"]——"dta" 距离 "data"=1
    /// 断言：Some{root=/data, tail=[f.md]}
    #[test]
    fn trie_fuzzy_root_name_correction() {
        let trie = RootTrie::build(&paths(&["/data", "/other"]));
        let result = trie.anchor(&comps("/dta/f.md"));
        let (root, tail) = result.expect("should fuzzy-correct to /data");
        assert_eq!(root, PathBuf::from("/data"));
        assert_eq!(tail, vec!["f.md".to_string()]);
    }

    /// P2：[RootTrie::anchor] root="/" 加上 tail
    /// 条件：roots=["/"]，input=["/","a","b.txt"]
    /// 断言：Some{root=/, tail=[a, b.txt]}
    #[test]
    fn trie_root_slash_with_tail() {
        let trie = RootTrie::build(&paths(&["/"]));
        let result = trie.anchor(&comps("/a/b.txt"));
        let (root, tail) = result.expect("should anchor at /");
        assert_eq!(root, PathBuf::from("/"));
        assert_eq!(tail, vec!["a".to_string(), "b.txt".to_string()]);
    }

    /// P2：[RootTrie::anchor] root="/" 无 tail（输入就是 "/"）
    /// 条件：roots=["/"]，input=["/"]
    /// 断言：Some{root=/, tail=[]}（anchor 找到但 tail 为空，由调用方拒绝）
    #[test]
    fn trie_root_slash_no_tail() {
        let trie = RootTrie::build(&paths(&["/"]));
        let (root, tail) = trie.anchor(&comps("/")).expect("should anchor at /");
        assert_eq!(root, PathBuf::from("/"));
        assert!(tail.is_empty(), "tail should be empty");
    }

    /// P2：[RootTrie::anchor] 共享前缀 root 中间层 aborts，取已到达的最深 root
    /// 条件：roots=["/a/b","/a/other"]，input=["/","a","b","c"]→到达 /a/b 后匹配 "c" 失败
    /// 断言：Some{root=/a/b}（"c" 失败但 /a/b 已是完整 root，tail=[c]）
    #[test]
    fn trie_shared_prefix_intermediate_abort_returns_deepest_complete() {
        let trie = RootTrie::build(&paths(&["/a/b"]));
        // /a/b is complete root. Input has "c" that can't match below /a/b
        // because /a/b has no children. pick returns Abort → loop breaks.
        // anchor was set to (2, /a/b) → tail=[c].
        let result = trie.anchor(&comps("/a/b/c"));
        let (root, tail) = result.expect("/a/b is complete, c is tail");
        assert_eq!(root, PathBuf::from("/a/b"));
        assert_eq!(tail, vec!["c".to_string()]);
    }

    /// P1：[RootTrie::build] 空路径段被静默跳过
    /// 条件：roots=["/workspace", ""]（含空路径）
    /// 断言：trie 仅包含 /workspace，anchor 正常返回
    #[test]
    fn trie_build_skips_empty_path() {
        let trie = RootTrie::build(&paths(&["/workspace", ""]));
        let result = trie.anchor(&comps("/workspace/f.md"));
        let (root, tail) = result.expect("empty path should be skipped, /workspace works");
        assert_eq!(root, PathBuf::from("/workspace"));
        assert_eq!(tail, vec!["f.md".to_string()]);
    }

    /// P1：[RootTrie::anchor] 输入段不以 "/" 开头返回 None
    /// 条件：roots=["/a"]，input=["a","b"]（首段非 root 分隔符）
    /// 断言：None（root 分隔符必须精确匹配，"a" 不等于 "/" 也不被模糊纠正）
    #[test]
    fn trie_anchor_no_root_prefix_returns_none() {
        let trie = RootTrie::build(&paths(&["/a"]));
        assert!(trie.anchor(&comps("a/b")).is_none());
    }

    // ── deepest_existing_ancestor ──

    use std::fs as stdfs;

    use tempfile::TempDir;

    /// P0：[deepest_existing_ancestor] 单层不存在的 tail 正确回溯
    /// 条件：路径 /a/b/c.md，其中 /a 存在、/a/b 不存在
    /// 断言：ancestor=/a, tail=[b, c.md]
    #[test]
    fn deepest_ancestor_one_level_nonexistent() {
        let tmp = TempDir::new().unwrap();
        // /tmp/a  exists, /tmp/a/b/c.md does not
        stdfs::create_dir(tmp.path().join("a")).unwrap();
        let path = tmp.path().join("a/b/c.md");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().join("a"));
        assert_eq!(tail, vec!["b".to_string(), "c.md".to_string()]);
    }

    /// P0：[deepest_existing_ancestor] 多层不存在 tail 全部回溯
    /// 条件：路径 /a/b/c/d.md，只有 /a 存在
    /// 断言：ancestor=/a, tail=[b, c, d.md]
    #[test]
    fn deepest_ancestor_multi_level_nonexistent() {
        let tmp = TempDir::new().unwrap();
        stdfs::create_dir(tmp.path().join("a")).unwrap();
        let path = tmp.path().join("a/b/c/d.md");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().join("a"));
        assert_eq!(
            tail,
            vec!["b".to_string(), "c".to_string(), "d.md".to_string(),]
        );
    }

    /// P1：[deepest_existing_ancestor] 路径完全存在返回 None（无 tail）
    /// 条件：目录 /a 存在且输入路径就是 /a
    /// 断言：返回 None
    #[test]
    fn deepest_ancestor_fully_existent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let result = deepest_existing_ancestor(tmp.path());
        assert!(result.is_none());
    }

    /// P1：[deepest_existing_ancestor] 文件视为可回溯的祖先（is_dir=false）
    /// 条件：路径 /a/f.txt/e.md，/a 存在但 /a/f.txt 是文件
    /// 断言：ancestor=/a, tail=[f.txt, e.md]
    #[test]
    fn deepest_ancestor_file_as_intermediate() {
        let tmp = TempDir::new().unwrap();
        stdfs::create_dir(tmp.path().join("a")).unwrap();
        stdfs::write(tmp.path().join("a/f.txt"), "data").unwrap();
        let path = tmp.path().join("a/f.txt/e.md");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().join("a"));
        assert_eq!(tail, vec!["f.txt".to_string(), "e.md".to_string()]);
    }

    /// P1：[deepest_existing_ancestor] 仅根目录存在、路径多级不存在
    /// 条件：路径 /a/b/c.md，无任何子目录被创建，只有 tempdir 根存在
    /// 断言：ancestor=tmp.path(), tail=[a, b, c.md]
    #[test]
    fn deepest_ancestor_only_root_with_multi_level() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/b/c.md");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().to_path_buf());
        assert_eq!(
            tail,
            vec!["a".to_string(), "b".to_string(), "c.md".to_string()]
        );
    }

    /// P1：[deepest_existing_ancestor] 路径是已存在的普通文件，回溯到父目录
    /// 条件：文件 /a/data.txt 已存在，输入路径即为该文件
    /// 断言：ancestor=/a, tail=[data.txt]
    #[test]
    fn deepest_ancestor_existing_file_walks_to_parent() {
        let tmp = TempDir::new().unwrap();
        stdfs::create_dir(tmp.path().join("a")).unwrap();
        stdfs::write(tmp.path().join("a/data.txt"), "hello").unwrap();
        let path = tmp.path().join("a/data.txt");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().join("a"));
        assert_eq!(tail, vec!["data.txt".to_string()]);
    }

    /// P1：[deepest_existing_ancestor] 仅单个非存在段，根目录存在
    /// 条件：路径 /c.md，仅根目录存在（c.md 不存在）
    /// 断言：ancestor=/tmp, tail=[c.md]
    #[test]
    fn deepest_ancestor_single_component() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.md");
        // c.md does not exist; only the tempdir root exists.

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().to_path_buf());
        assert_eq!(tail, vec!["c.md".to_string()]);
    }

    /// P2：[deepest_existing_ancestor] 深层不存在的路径回溯到浅层祖先
    /// 条件：路径 /a/b/c/d/e/f.md，只有 /a 存在
    /// 断言：ancestor=/a, tail=[b, c, d, e, f.md]
    #[test]
    fn deepest_ancestor_deep_nonexistent() {
        let tmp = TempDir::new().unwrap();
        stdfs::create_dir(tmp.path().join("a")).unwrap();
        let path = tmp.path().join("a/b/c/d/e/f.md");

        let (ancestor, tail) = deepest_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, tmp.path().join("a"));
        assert_eq!(
            tail,
            vec![
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f.md".to_string(),
            ]
        );
    }
}
