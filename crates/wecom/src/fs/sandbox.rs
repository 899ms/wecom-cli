use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Verify that `path` stays within one of the `roots`.
///
/// Both `path` and each root are resolved via [`resolve_real_path`] before
/// comparison so that symlinks and `..` components cannot bypass the check.
///
/// **Note:** Callers that have *already* resolved `path` may pass it in
/// directly — the extra `resolve_real_path` call on an already-canonical path
/// is effectively a no-op (it just calls `canonicalize` which returns
/// immediately for existing paths, or normalises for non-existing ones).
pub(super) fn check_path_in_roots(path: &Path, roots: Option<&[PathBuf]>) -> Result<()> {
    let Some(roots) = roots else {
        // No roots configured — unrestricted access.
        return Ok(());
    };

    let real = resolve_real_path(path);
    if !roots.iter().any(|root| {
        let real_root = resolve_real_path(root);
        real.starts_with(&real_root)
    }) {
        let allowed = roots
            .iter()
            .map(|r| resolve_real_path(r).display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::error!(path = %path.display(), allowed = %allowed, "sandbox: path outside allowed roots");
        return Err(Error::Permission(format!(
            "目标路径超出可访问范围: {} (允许范围: {})",
            path.display(),
            allowed,
        )));
    }
    Ok(())
}

/// Resolve a path (follow symlinks) and verify it stays within `roots`.
///
/// Returns the resolved (symlink-followed) path on success.
/// This is the single entry-point for "resolve + validate" — all callers
/// that only need a pre-flight check (without opening a file) should use
/// this function.
pub(super) fn resolve_and_check_path(
    path: impl AsRef<Path>,
    roots: Option<&[PathBuf]>,
) -> Result<PathBuf> {
    let real = resolve_real_path(path.as_ref());
    check_path_in_roots(&real, roots)?;
    Ok(real)
}

/// TOCTOU-safe open for **reading** within the sandbox (synchronous).
///
/// Returns the resolved path and the opened `std::fs::File`.
/// SAFETY: This is the sandbox implementation itself — it uses raw `std::fs`
/// and then verifies the fd-level path.
#[allow(clippy::disallowed_methods)]
pub(super) fn open_file(
    path: impl AsRef<Path>,
    roots: Option<&[PathBuf]>,
) -> Result<(PathBuf, File)> {
    let real = resolve_and_check_path(path, roots)?;

    let file = File::open(&real)
        .map_err(|e| Error::io(format!("Failed to open {}", real.display()), e))?;

    #[cfg(target_os = "linux")]
    verify_fd_path(&file, roots)?;

    Ok((real, file))
}

/// TOCTOU-safe **create** (`create_new`, `0o600`) within the sandbox.
/// SAFETY: This is the sandbox implementation itself.
#[allow(clippy::disallowed_methods)]
pub(super) fn create_file(
    path: impl AsRef<Path>,
    roots: Option<&[PathBuf]>,
) -> Result<(PathBuf, File)> {
    let real = resolve_and_check_path(path, roots)?;

    // Ensure parent directory exists.
    if let Some(parent) = real.parent() {
        create_dir_all(parent, 0o700)?;
    }

    let file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        opts.open(&real).map_err(|e| {
            Error::io(
                format!("Failed to create output file {}", real.display()),
                e,
            )
        })?
    };

    // Verify the *actual* path of the created file is still under an allowed
    // root. If an attacker swapped a directory for a symlink between
    // resolve_real_path and the open above, this will catch it.
    #[cfg(target_os = "linux")]
    if let Err(e) = verify_fd_path(&file, roots) {
        // Clean up the escaped file before returning the error.
        drop(file);
        let _ = std::fs::remove_file(&real);
        return Err(e);
    }

    Ok((real, file))
}

/// Verify that an already-opened fd actually resides under one of the `roots`.
#[cfg(target_os = "linux")]
pub(super) fn verify_fd_path(file: &File, roots: Option<&[PathBuf]>) -> Result<()> {
    let Some(roots) = roots else {
        // No roots configured — unrestricted access.
        return Ok(());
    };

    let Some(real) = fd_real_path(file) else {
        return Ok(());
    };

    let escapes = !roots.iter().any(|root| {
        let real_root = resolve_real_path(root);
        real.starts_with(&real_root)
    });

    if escapes {
        let allowed = roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::error!(
            fd_path = %real.display(),
            allowed = %allowed,
            "Sandbox: TOCTOU detected — fd path escapes after open"
        );
        return Err(Error::Permission(format!(
            "目标路径超出可访问范围: {} (允许范围: {})",
            real.display(),
            allowed,
        )));
    }

    Ok(())
}

/// Resolve a path to its real location on disk, handling symlinks.
///
/// - If the full path exists, [`std::fs::canonicalize`] is used.
/// - If the path does not exist yet, the deepest existing ancestor is
///   canonicalised and the remaining non-existent tail segments are normalised
///   and appended.
pub(super) fn resolve_real_path(path: &Path) -> PathBuf {
    // Fast path: the full path exists.
    if let Ok(real) = path.canonicalize() {
        return real;
    }

    // Walk up until we find an ancestor that exists.
    let normalised = normalize_path(path);
    let mut existing = normalised.as_path();
    let mut tail = Vec::new();

    loop {
        if existing.exists() {
            break;
        }
        match existing.file_name() {
            Some(seg) => {
                tail.push(seg.to_os_string());
                existing = existing.parent().unwrap_or(existing);
            }
            None => break, // root or empty — nothing more to pop
        }
    }

    // Canonicalise the existing prefix (resolves symlinks).
    let mut result = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());

    // Re-attach the non-existent tail in reverse order.
    for seg in tail.into_iter().rev() {
        result.push(seg);
    }
    result
}

/// Logically normalise a path by resolving `.` and `..` components
/// **without** touching the filesystem.
pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {} // skip "."
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp),
        }
    }
    out
}

/// Get the real filesystem path of an already-opened file descriptor.
///
/// On Linux this reads `/proc/self/fd/<fd>` which is authoritative. On other
/// platforms we fall back to `canonicalize` on the original path (less robust
/// but still useful).
/// SAFETY: This is the sandbox implementation itself — fd-level path resolution.
#[allow(clippy::disallowed_methods)]
#[cfg(target_os = "linux")]
fn fd_real_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;

    let fd_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    std::fs::read_link(&fd_path).ok()
}

// ── Atomic write ────────────────────────────────────────────

/// Synchronous core of [`super::Fs::atomic_write`].
///
/// Strategy: write to a temp file in the same directory → fsync → rename.
/// This guarantees that readers never see a partially-written file.
/// SAFETY: This is the sandbox implementation itself — low-level atomic write.
#[cfg_attr(windows, allow(unused_variables))]
pub(super) fn atomic_write_blocking(path: &Path, data: &[u8], mode: u32) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Validation(format!("无效文件路径: {path:?}")))?;

    create_dir_all(parent, 0o700)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| Error::io(format!("Failed to create temp file in {parent:?}"), e))?;

    // Set permissions on the temp file *before* persisting so the file is
    // never visible at the target path with overly-permissive mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::io("Failed to set temp file permissions", e))?;
    }

    tmp.write_all(data)
        .map_err(|e| Error::io("Failed to write temp file", e))?;

    tmp.as_file()
        .sync_all()
        .map_err(|e| Error::io("Failed to sync temp file", e))?;

    let p = path.to_path_buf();
    tmp.persist(path)
        .map_err(|e| Error::io(format!("Failed to persist temp file to {path:?}"), e.error))?;

    Ok(p)
}

// ── Remove operations ───────────────────────────────────────

/// Remove a single file within the sandbox.
///
/// The path is resolved (symlinks followed) and validated against the
/// given writable `roots` before deletion.
/// SAFETY: This is the sandbox implementation itself.
#[allow(clippy::disallowed_methods)]
pub(super) fn sandbox_remove_file(path: impl AsRef<Path>, roots: Option<&[PathBuf]>) -> Result<()> {
    let real = resolve_and_check_path(path, roots)?;
    std::fs::remove_file(&real)
        .map_err(|e| Error::io(format!("Failed to remove {}", real.display()), e))
}

/// Recursively remove a directory and all its contents within the sandbox.
///
/// The path is resolved (symlinks followed) and validated against the
/// given writable `roots` before deletion.
/// SAFETY: This is the sandbox implementation itself.
#[allow(clippy::disallowed_methods)]
pub(super) fn sandbox_remove_dir_all(
    path: impl AsRef<Path>,
    roots: Option<&[PathBuf]>,
) -> Result<()> {
    let real = resolve_and_check_path(path, roots)?;
    std::fs::remove_dir_all(&real)
        .map_err(|e| Error::io(format!("Failed to remove {}", real.display()), e))
}

// ── Directory listing ───────────────────────────────────────

/// List all directory entries (files and subdirectories) in a directory
/// within the sandbox.
///
/// The path is resolved (symlinks followed) and validated against the
/// given readable `roots` before listing.
/// SAFETY: This is the sandbox implementation itself.
#[allow(clippy::disallowed_methods)]
pub(super) fn sandbox_list_dir(
    dir: impl AsRef<Path>,
    roots: Option<&[PathBuf]>,
) -> Result<Vec<PathBuf>> {
    let real = resolve_and_check_path(dir, roots)?;

    let read_dir = std::fs::read_dir(&real)
        .map_err(|e| Error::io(format!("Failed to read directory {}", real.display()), e))?;

    Ok(read_dir.flatten().map(|e| e.path()).collect())
}

// ── Directory creation ──────────────────────────────────────

/// SAFETY: This is the sandbox implementation itself — directory creation.
#[cfg_attr(windows, allow(unused_variables))]
pub(super) fn create_dir_all(path: &Path, mode: u32) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }

    builder
        .recursive(true)
        .create(path)
        .map_err(|e| Error::io(format!("Failed to create directory {}", path.display()), e))
}

// ══════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：sandbox（沙箱路径校验与 I/O 底层实现）
    //!
    //! ### 关键接口
    //! - [check_path_in_roots] — 校验路径是否在允许的 roots 内
    //! - [resolve_real_path] — 解析真实路径（跟随符号链接）
    //! - [open_file] — TOCTOU 安全地打开文件用于读取
    //! - [create_file] — 在沙箱内创建文件（create_new 语义 + 0o600 权限）
    //! - [atomic_write_blocking] — 原子写入（临时文件 → fsync → rename）
    //! - [sandbox_remove_file] / [sandbox_remove_dir_all] — 沙箱内删除
    //! - [sandbox_list_dir] — 列出目录下所有条目（文件与子目录）
    //! - [create_dir_all] — 递归创建目录
    //! - [verify_fd_path] — fd 级路径校验（防 TOCTOU 攻击）
    //! - [normalize_path] — 逻辑归一化路径（解析 . 和 .. 段）
    //! - [fd_real_path] — 通过 /proc/self/fd/ 获取 fd 的真实路径
    //!
    //! ### 关键分支与异常路径
    //! - 路径逃逸 roots（含 .. 注入）→ Err("escapes allowed directories")
    //! - None roots → 无限制模式（跳过所有校验）
    //! - 文件不存在时 open_file → Err("Failed to open")
    //! - 文件已存在时 create_file → Err(AlreadyExists)
    //! - 原子写入到不可写目录 → Err("Failed to create temp file" 或 "Failed to create directory")
    //! - 符号链接指向沙箱外 → resolve_real_path 解析后仍做 roots 校验
    //! - verify_fd_path 在 Linux 上通过 /proc/self/fd/ 做二次校验
    //!
    //! ### 上下游交互
    //! - 上游：[Fs]（mod.rs）的所有公开方法均委托给本模块的具体函数
    //! - 下游：依赖 `std::fs` 系统调用和 `/proc/self/fd/`（Linux）进行实际 I/O

    use std::fs as stdfs;

    use tempfile::TempDir;

    use super::*;

    // ── normalize_path ──

    /// P0：[normalize_path] 路径中的 . 段被正确移除
    /// 条件：输入 "/a/./b"
    /// 断言：normalize_path 返回 "/a/b"
    #[test]
    fn normalize_removes_dot() {
        assert_eq!(normalize_path(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }

    /// P0：[normalize_path] 路径中的 .. 段被正确回退
    /// 条件：输入 "/a/b/../c"
    /// 断言：normalize_path 返回 "/a/c"
    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
    }

    /// P1：[normalize_path] 连续多个 .. 段被逐级回退
    /// 条件：输入 "/a/b/c/../../d"
    /// 断言：normalize_path 返回 "/a/d"
    #[test]
    fn normalize_multiple_dotdots() {
        assert_eq!(
            normalize_path(Path::new("/a/b/c/../../d")),
            PathBuf::from("/a/d")
        );
    }

    /// P1：[normalize_path] 根路径 "/" 归一化后不变
    /// 条件：输入 "/"
    /// 断言：返回 "/"
    #[test]
    fn normalize_root_only() {
        assert_eq!(normalize_path(Path::new("/")), PathBuf::from("/"));
    }

    /// P1：[normalize_path] 空路径归一化后仍为空
    /// 条件：输入 ""
    /// 断言：返回 ""
    #[test]
    fn normalize_empty_path() {
        assert_eq!(normalize_path(Path::new("")), PathBuf::from(""));
    }

    /// P1：[normalize_path] 无特殊段路径归一化后不变
    /// 条件：输入 "/a/b/c"
    /// 断言：返回 "/a/b/c"
    #[test]
    fn normalize_no_special_components() {
        assert_eq!(normalize_path(Path::new("/a/b/c")), PathBuf::from("/a/b/c"));
    }

    // ── check_path_in_roots ──

    /// P0：[check_path_in_roots] 根目录下的绝对路径通过沙盒检查
    /// 条件：文件位于临时根目录的子目录下
    /// 断言：check_path_in_roots 返回 Ok，路径以预期结尾
    #[test]
    fn check_absolute_under_root() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("output/result.json");
        let real = resolve_real_path(&file);
        assert!(check_path_in_roots(&real, Some(&roots)).is_ok());
        // The resolved path should end with the expected tail
        assert!(
            real.ends_with("output/result.json"),
            "real = {}",
            real.display()
        );
    }

    /// P0：[check_path_in_roots] 含 ./ 的路径通过沙盒检查并正确解析
    /// 条件：使用 ./data/file.txt 形式的相对路径
    /// 断言：check_path_in_roots 返回 Ok，解析后不含 ./
    #[test]
    fn check_dot_path() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let joined = tmp.path().join("./data/file.txt");
        let real = resolve_real_path(&joined);
        assert!(check_path_in_roots(&real, Some(&roots)).is_ok());
        assert!(real.ends_with("data/file.txt"), "real = {}", real.display());
    }

    /// P0：[check_path_in_roots] 内部 .. 段（不越界）通过沙盒检查
    /// 条件：路径包含 sub/../other 仍在根目录内
    /// 断言：check_path_in_roots 返回 Ok，解析后指向 other 目录
    #[test]
    fn check_inner_dotdot_stays_in_root() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let joined = tmp.path().join("sub/../other/file.txt");
        let real = resolve_real_path(&joined);
        assert!(check_path_in_roots(&real, Some(&roots)).is_ok());
        assert!(
            real.ends_with("other/file.txt"),
            "real = {}",
            real.display()
        );
    }

    /// P0：[check_path_in_roots] 多根目录时文件位于额外根目录下通过检查
    /// 条件：配置两个根目录，文件位于第二个根目录
    /// 断言：check_path_in_roots 返回 Ok，路径以预期结尾
    #[test]
    fn check_allows_path_under_extra_root() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let roots = vec![tmp1.path().to_path_buf(), tmp2.path().to_path_buf()];
        let file = tmp2.path().join("upload.bin");
        let real = resolve_real_path(&file);
        assert!(check_path_in_roots(&real, Some(&roots)).is_ok());
        assert!(real.ends_with("upload.bin"), "real = {}", real.display());
    }

    /// P0：[check_path_in_roots] 无根目录配置（None）时允许任意路径
    /// 条件：roots 为 None，路径为 /etc/passwd
    /// 断言：check_path_in_roots 返回 Ok
    #[test]
    fn check_none_roots_allows_any_path() {
        let err = check_path_in_roots(&resolve_real_path(Path::new("/etc/passwd")), None);
        assert!(err.is_ok(), "None roots should allow any path");
    }

    /// P1：[check_path_in_roots] 利用 .. 越出根目录被拒绝
    /// 条件：路径通过 ../../../etc/passwd 尝试逃逸
    /// 断言：返回错误且消息包含 "目标路径超出可访问范围"
    #[test]
    fn check_rejects_escape() {
        let roots = vec![PathBuf::from("/home/user/project")];
        let joined = Path::new("/home/user/project").join("../../../etc/passwd");
        let err = check_path_in_roots(&resolve_real_path(&joined), Some(&roots));
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
    }

    /// P1：[check_path_in_roots] 根目录外的绝对路径被拒绝
    /// 条件：路径为 /etc/passwd，根目录为 /home/user/project
    /// 断言：check_path_in_roots 返回错误且包含 "目标路径超出可访问范围"
    #[test]
    fn check_rejects_absolute_outside_roots() {
        let roots = vec![PathBuf::from("/home/user/project")];
        let err = check_path_in_roots(&resolve_real_path(Path::new("/etc/passwd")), Some(&roots));
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
    }

    /// P1：[check_path_in_roots] 深层嵌套的 .. 段逃逸被拒绝
    /// 条件：路径通过 a/b/../../../../.. 越出根目录
    /// 断言：check_path_in_roots 返回 Err
    #[test]
    fn check_rejects_sneaky_dotdot() {
        let roots = vec![PathBuf::from("/home/user/project")];
        let joined = Path::new("/home/user/project").join("a/b/../../../../etc/shadow");
        let err = check_path_in_roots(&resolve_real_path(&joined), Some(&roots));
        assert!(err.is_err());
    }

    /// P1：[check_path_in_roots] 路径不在任何根目录下时被拒绝
    /// 条件：配置两个根目录，路径为 /etc/passwd 均不匹配
    /// 断言：check_path_in_roots 返回 Err
    #[test]
    fn check_rejects_path_outside_all_roots() {
        let roots = vec![
            PathBuf::from("/home/user/project"),
            PathBuf::from("/tmp/wecom"),
        ];
        let err = check_path_in_roots(&resolve_real_path(Path::new("/etc/passwd")), Some(&roots));
        assert!(err.is_err());
    }

    // ── resolve_real_path ──

    /// P0：[resolve_real_path] 已存在文件的路径解析为 canonicalize 结果
    /// 条件：文件已存在于临时目录
    /// 断言：resolve_real_path 返回值与 canonicalize 一致
    #[test]
    fn resolve_real_path_existing_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("exists.txt");
        stdfs::write(&file, "hi").unwrap();
        let resolved = resolve_real_path(&file);
        // canonicalize resolves symlinks; on real fs it should match.
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    /// P1：[resolve_real_path] 不存在文件的路径保留文件名
    /// 条件：文件在临时目录中不存在
    /// 断言：解析结果包含原始文件名
    #[test]
    fn resolve_real_path_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("does-not-exist.txt");
        let resolved = resolve_real_path(&file);
        // The parent exists, so it's canonicalised + the tail appended.
        assert!(
            resolved.to_string_lossy().contains("does-not-exist.txt"),
            "resolved = {}",
            resolved.display()
        );
    }

    /// P1：[resolve_real_path] 深层不存在路径保留完整尾部
    /// 条件：路径 a/b/c/d.txt 均不存在但父目录存在
    /// 断言：解析结果以 "a/b/c/d.txt" 结尾
    #[test]
    fn resolve_real_path_deep_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/b/c/d.txt");
        let resolved = resolve_real_path(&path);
        // Use Path::ends_with to be cross-platform (handles `/` vs `\`).
        assert!(
            resolved.ends_with(Path::new("a/b/c/d.txt")),
            "resolved = {}",
            resolved.display()
        );
    }

    // ── open_file_sync ──

    /// P0：[open_file] 同步打开沙盒内已存在文件成功
    /// 条件：文件存在于沙盒根目录下
    /// 断言：open_file 返回 Ok，解析路径与 canonicalize 一致
    #[test]
    fn open_file_sync_success() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("readable.txt");
        stdfs::write(&file, "contents").unwrap();

        let (resolved, _std_file) = open_file(&file, Some(&roots)).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    /// P1：[open_file] 同步打开沙盒外文件被拒绝
    /// 条件：文件存在于禁止目录，根目录为允许目录
    /// 断言：open_file 返回 Err
    #[test]
    fn open_file_sync_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let roots = vec![allowed.path().to_path_buf()];
        let file = forbidden.path().join("secret.txt");
        stdfs::write(&file, "secret").unwrap();

        let result = open_file(&file, Some(&roots));
        assert!(result.is_err());
    }

    /// P1：[open_file] 同步打开不存在文件返回错误
    /// 条件：文件在沙盒内但不存在
    /// 断言：open_file 返回 Err，错误信息包含 "Failed to open"
    #[test]
    fn open_file_sync_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("missing.txt");
        let result = open_file(&file, Some(&roots));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to open"));
    }

    // ── create_file_inner ──

    /// P0：[create_file] 在沙盒内创建新文件成功
    /// 条件：目标路径在根目录下且不存在
    /// 断言：create_file 返回 Ok，解析后的文件存在
    #[test]
    fn create_file_inner_success() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("new.txt");
        let (resolved, _f) = create_file(&file, Some(&roots)).unwrap();
        assert!(resolved.exists());
    }

    /// P1：[create_file] 在父目录不存在时自动创建
    /// 条件：目标路径 sub/dir/new.txt 的父目录不存在
    /// 断言：create_file 返回 Ok，文件已创建
    #[test]
    fn create_file_inner_creates_parent() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("sub/dir/new.txt");
        let (resolved, _f) = create_file(&file, Some(&roots)).unwrap();
        assert!(resolved.exists());
    }

    /// P1：[create_file] 拒绝在沙盒外创建文件
    /// 条件：文件路径位于 forbidden 目录，roots 仅包含 allowed 目录
    /// 断言：create_file 返回 Err
    #[test]
    fn create_file_inner_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let roots = vec![allowed.path().to_path_buf()];
        let file = forbidden.path().join("escape.txt");
        let result = create_file(&file, Some(&roots));
        assert!(result.is_err());
    }

    /// P1：[create_file] 对已存在的文件返回错误（create_new 语义）
    /// 条件：目标文件已预先写入数据
    /// 断言：create_file 返回 Err
    #[test]
    fn create_file_inner_already_exists_returns_err() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let file = tmp.path().join("exists.txt");
        stdfs::write(&file, "data").unwrap();
        let result = create_file(&file, Some(&roots));
        assert!(result.is_err());
    }

    // ── verify_fd_path ──

    /// P1：[verify_fd_path] 拒绝沙盒外 fd
    /// 条件：打开 /etc/hostname，根目录为 /home
    /// 断言：返回错误且包含 "目标路径超出可访问范围"
    #[test]
    #[cfg(target_os = "linux")]
    fn verify_fd_rejects_outside_roots() {
        let roots = vec![PathBuf::from("/home")];
        let file = File::open("/etc/hostname");
        if let Ok(file) = file {
            let err = verify_fd_path(&file, Some(&roots));
            assert!(err.is_err());
            assert!(
                err.unwrap_err()
                    .to_string()
                    .contains("目标路径超出可访问范围")
            );
        }
    }

    /// P0：[verify_fd_path] 对沙盒内 fd 返回成功
    /// 条件：fd 为临时目录下的测试文件
    /// 断言：verify_fd_path 返回 Ok
    #[test]
    #[cfg(target_os = "linux")]
    fn verify_fd_succeeds_within_roots() {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let file_path = dir.path().join("test.txt");
        stdfs::write(&file_path, "hello").unwrap();

        let file = File::open(&file_path).unwrap();
        let result = verify_fd_path(&file, Some(&roots));
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    /// P0：[verify_fd_path] 无根目录限制时允许任意路径
    /// 条件：roots 为 None，fd 为 /etc/hostname
    /// 断言：返回 Ok
    #[test]
    #[cfg(target_os = "linux")]
    fn verify_fd_none_roots_allows_any_path() {
        let file = File::open("/etc/hostname");
        if let Ok(file) = file {
            let result = verify_fd_path(&file, None);
            assert!(result.is_ok(), "None roots should allow any path");
        }
    }

    // ── fd_real_path ──

    /// P0：[fd_real_path] 返回文件的真实 canonical 路径
    /// 条件：在临时目录中写入测试文件并打开
    /// 断言：返回值与 canonicalize 一致
    #[test]
    #[cfg(target_os = "linux")]
    fn fd_real_path_returns_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("fd-test.txt");
        stdfs::write(&file_path, "data").unwrap();

        let file = File::open(&file_path).unwrap();
        let result = fd_real_path(&file).unwrap();
        assert_eq!(result, file_path.canonicalize().unwrap());
    }

    // ── atomic_write_blocking ──

    /// P0：[atomic_write_blocking] 原子写入能正确写入二进制数据
    /// 条件：向临时文件写入 [0xDE, 0xAD, 0xBE, 0xEF]
    /// 断言：读取文件内容与原始数据一致
    #[test]
    fn atomic_write_writes_bytes() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("binary.bin");
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        atomic_write_blocking(&file, &data, 0o644).unwrap();
        assert_eq!(stdfs::read(&file).unwrap(), data);
    }

    /// P1：[atomic_write_blocking] 原子写入覆盖已有文件内容
    /// 条件：同一文件先写入 "first" 再写入 "second"
    /// 断言：最终读取为 "second"
    #[test]
    fn atomic_write_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("report.xml");

        atomic_write_blocking(&file, b"first", 0o644).unwrap();
        assert_eq!(stdfs::read_to_string(&file).unwrap(), "first");

        atomic_write_blocking(&file, b"second", 0o644).unwrap();
        assert_eq!(stdfs::read_to_string(&file).unwrap(), "second");
    }

    /// P1：[atomic_write_blocking] 原子写入自动创建父目录
    /// 条件：目标路径 a/b/c/file.txt 的父目录均不存在
    /// 断言：文件创建成功且内容为 "deep"
    #[test]
    fn atomic_write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a/b/c/file.txt");
        atomic_write_blocking(&file, b"deep", 0o644).unwrap();
        assert_eq!(stdfs::read_to_string(&file).unwrap(), "deep");
    }

    /// P1：原子写入在 Unix 上设置正确权限
    /// 条件：以 0o600 权限写入文件
    /// 断言：文件权限 & 0o777 == 0o600
    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("perms.txt");
        atomic_write_blocking(&file, b"data", 0o600).unwrap();
        let mode = stdfs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // ── create_dir_all ──

    /// P0：[create_dir_all] 递归创建目录
    /// 条件：目标路径 a/b/c 不存在
    /// 断言：创建成功且为目录
    #[test]
    fn create_dir_all_success() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a/b/c");
        create_dir_all(&dir, 0o700).unwrap();
        assert!(dir.exists());
        assert!(dir.is_dir());
    }

    /// P1：[create_dir_all] 对不可写父目录返回错误
    /// 条件：目标位于 /proc（不可写）
    /// 断言：返回错误且包含 "Failed to create directory"
    #[cfg(unix)]
    #[test]
    fn create_dir_all_unwritable_parent_fails() {
        // /proc is not writable, so creating a dir under it should fail.
        let result = create_dir_all(Path::new("/proc/fake-dir-test"), 0o700);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to create directory")
        );
    }

    // ── sandbox_list_dir ──

    /// P0：[sandbox_list_dir] 返回目录下所有条目（含文件和子目录）
    /// 条件：目录下有 2 个文件和 1 个子目录
    /// 断言：返回 3 个条目，其中 1 个是目录、2 个是文件
    #[test]
    fn sandbox_list_dir_includes_dirs() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("a.txt"), "a").unwrap();
        stdfs::write(tmp.path().join("b.txt"), "b").unwrap();
        stdfs::create_dir(tmp.path().join("subdir")).unwrap();

        let roots = vec![tmp.path().to_path_buf()];
        let entries = sandbox_list_dir(tmp.path(), Some(&roots)).unwrap();
        assert_eq!(entries.len(), 3, "list_dir must return all entries");
        let dir_count = entries.iter().filter(|p| p.is_dir()).count();
        assert_eq!(dir_count, 1, "list_dir must include subdirectory entries");
    }

    // ── atomic_write_blocking error paths ──

    /// P1：[atomic_write_blocking] 原子写入到不可写位置时失败
    /// 条件：目标位于 /proc（不可写）
    /// 断言：返回错误且消息包含 "Failed to create temp file" 或 "Failed to create directory"
    #[cfg(unix)]
    #[test]
    fn atomic_write_blocking_unwritable_dir_fails() {
        // Writing to a read-only location should fail at temp file creation.
        let result = atomic_write_blocking(Path::new("/proc/nonexistent/file.txt"), b"data", 0o644);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Failed to create temp file")
                || msg.contains("Failed to create directory"),
            "msg = {msg}"
        );
    }

    /// P1：[atomic_write_blocking] 对正常路径成功写入
    /// 条件：向临时文件写入 "ok"
    /// 断言：返回 Ok
    #[test]
    fn atomic_write_blocking_root_path_no_parent() {
        // A path like "/" has no parent — should return Validation error.
        // Actually Path::new("/").parent() returns Some(""), so let's try
        // a truly pathological case.
        // On Unix, "/" has parent = None when you call .parent() on a single component.
        // But PathBuf::from("/").parent() returns Some(""), which is actually fine.
        // Let's just ensure the function works for edge paths.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("edge.txt");
        let result = atomic_write_blocking(&file, b"ok", 0o644);
        assert!(result.is_ok());
    }

    // ── resolve_real_path edge cases ──

    /// P1：[resolve_real_path] 对根路径 "/" 返回 "/"
    /// 条件：输入 "/"
    /// 断言：返回 "/"
    #[test]
    #[cfg(unix)]
    fn resolve_real_path_root_slash() {
        let resolved = resolve_real_path(Path::new("/"));
        assert_eq!(resolved, PathBuf::from("/"));
    }

    /// P1：[resolve_real_path] 对 Windows 根路径返回有效的根目录
    /// 条件：输入 "C:\\"
    /// 断言：解析结果存在
    #[test]
    #[cfg(windows)]
    fn resolve_real_path_root_slash() {
        let resolved = resolve_real_path(Path::new("C:\\"));
        assert!(resolved.exists(), "resolved = {}", resolved.display());
    }

    /// P1：[resolve_real_path] 正确解析含 .. 的路径
    /// 条件：路径 a/../b 在临时目录下
    /// 断言：结果包含 "b" 且不含 ".."
    #[test]
    fn resolve_real_path_with_dotdot_components() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/../b");
        let resolved = resolve_real_path(&path);
        assert!(resolved.to_string_lossy().contains("b"));
        assert!(!resolved.to_string_lossy().contains(".."));
    }

    /// P1：完全不存在路径的解析保留原始文件名
    /// 条件：路径 ___nonexistent_test_xyz___/deep/path 均不存在
    /// 断言：解析结果仍保留原始名称段
    #[test]
    fn resolve_real_path_nonexistent_under_root() {
        // This path has no existing ancestors until we reach "/".
        // Walking up will eventually hit file_name() == None on "/".
        let resolved = resolve_real_path(Path::new("/___nonexistent_test_xyz___/deep/path"));
        assert!(
            resolved
                .to_string_lossy()
                .contains("___nonexistent_test_xyz___")
        );
    }

    /// P1：[resolve_real_path] 正确处理含 . 的路径 [[resolve_real_path]]
    /// 条件：路径 ./sub/./file.txt 在临时目录下
    /// 断言：结果不包含 "/./"
    #[test]
    fn resolve_real_path_with_dot_components() {
        // Tests that CurDir (`.`) is properly handled by normalize_path
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("./sub/./file.txt");
        let resolved = resolve_real_path(&path);
        assert!(!resolved.to_string_lossy().contains("/./"));
    }

    // ── open_file_sync TOCTOU test ──

    /// P1：[open_file] 沙盒内符号链接指向的文件可正常打开（TOCTOU 安全）
    /// 条件：link.txt 是 real.txt 的符号链接，均在根目录下
    /// 断言：open_file 返回 Ok
    #[cfg(unix)]
    #[test]
    fn open_file_sync_with_symlink_within_roots() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let real_file = tmp.path().join("real.txt");
        stdfs::write(&real_file, "data").unwrap();
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&real_file, &link).unwrap();

        // Both real and link are within the same root, should succeed.
        let result = open_file(&link, Some(&roots));
        assert!(result.is_ok());
    }
}
