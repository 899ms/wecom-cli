mod fuzzy;
mod reqwest_ext;
mod sandbox;
mod sanitize;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::{JoinHandle, spawn_blocking};

use crate::telemetry::contract::path_fuzzy_corrected as ctr;
use crate::{Error, Result, telemetry, util};

/// Map a caller-supplied path (relative, absolute, or even a non-filesystem
/// path such as `virtual://workspace/...`) to an **absolute physical path**.
///
/// # Contract
///
/// - Synchronous, cheap, idempotent; **no blocking I/O** (called from async
///   threads).
/// - The return value **must be an absolute physical path**; downstream
///   sandbox validation and fuzzy correction are based on it.
/// - Return `Err` when mapping is impossible (typically
///   [`Error::Validation`] / [`Error::Permission`]).
pub type PathResolver = Arc<dyn Fn(&Path) -> Result<PathBuf> + Send + Sync>;

/// Sandboxed file-system handle that restricts all I/O to a set of allowed
/// directory roots.
///
/// `Fs` does **not** depend on [`super::Client`] — it only needs a
/// working directory (`cwd`, used to resolve relative paths) and a list of
/// allowed roots (used for security validation).  This makes it usable in
/// contexts where no `Client` exists yet (e.g. credential decryption during
/// client construction).
///
/// Read and write operations are validated against **separate** root lists:
///
/// - **Readable roots** — used by read-only operations (`read_to_string`,
///   `metadata`, `open_as_multipart_part`, `list_dir`).
/// - **Writable roots** — used by operations that create or modify files
///   (`create_file`, `atomic_write`, `remove_file`, `remove_dir_all`).
///
/// When both root lists are `None`, no path restrictions are applied
/// (unrestricted mode).
///
/// By default [`new`] creates an unrestricted instance.
/// Use [`new_with_permissions`] to supply readable / writable root lists
/// (pass `Some` to restrict, `None` to leave unrestricted).
///
/// When a [`Client`](super::Client) *is* available, obtain a pre-configured
/// instance via [`Client::fs()`](super::Client::fs).
///
/// # Example
///
/// ```ignore
/// // Unrestricted:
/// let fs = Fs::new("/project");
///
/// // Equal read/write roots:
/// let fs = Fs::new_with_permissions(
///     "/project",
///     Some(&["/project", "/home/user/.config"]),
///     Some(&["/project", "/home/user/.config"]),
/// );
///
/// // Separate read/write roots:
/// let fs = Fs::new_with_permissions(
///     "/project",
///     Some(&["/project", "/data/readonly"]),   // readable
///     Some(&["/project"]),                      // writable
/// );
/// ```
pub struct Fs {
    cwd: PathBuf,
    readable_dirs: Option<Vec<PathBuf>>,
    writable_dirs: Option<Vec<PathBuf>>,
    resolver: Option<PathResolver>,
}

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs")
            .field("cwd", &self.cwd)
            .field("readable_dirs", &self.readable_dirs)
            .field("writable_dirs", &self.writable_dirs)
            .field("resolver", &self.resolver.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

impl Fs {
    /// Create a new **unrestricted** `Fs` scoped to the given `cwd`.
    ///
    /// `cwd` is used to resolve relative paths.  No path restrictions are
    /// applied — all read and write operations are allowed regardless of
    /// location.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            readable_dirs: None,
            writable_dirs: None,
            resolver: None,
        }
    }

    /// Create a new `Fs` with **separate** readable and writable root
    /// lists.
    ///
    /// `cwd` is used to resolve relative paths.
    ///
    /// - `readable_roots` — directories from which files may be **read**.
    ///   Pass `None` for unrestricted reading.
    /// - `writable_roots` — directories in which files may be **created,
    ///   written, or deleted**.
    ///   Pass `None` for unrestricted writing.
    ///
    /// A directory that should be both readable and writable must appear in
    /// **both** lists.
    pub fn new_with_permissions(
        cwd: impl Into<PathBuf>,
        readable_dirs: Option<&[&Path]>,
        writable_dirs: Option<&[&Path]>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            readable_dirs: readable_dirs.map(|s| s.iter().map(|p| p.to_path_buf()).collect()),
            writable_dirs: writable_dirs.map(|s| s.iter().map(|p| p.to_path_buf()).collect()),
            resolver: None,
        }
    }

    /// Current working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn set_cwd(&mut self, cwd: impl Into<PathBuf>) {
        self.cwd = cwd.into();
    }

    pub fn readable_dirs(&self) -> Option<&[PathBuf]> {
        self.readable_dirs.as_deref()
    }

    pub fn readable_dirs_mut(&mut self) -> &mut Option<Vec<PathBuf>> {
        &mut self.readable_dirs
    }

    pub fn writable_dirs(&self) -> Option<&[PathBuf]> {
        self.writable_dirs.as_deref()
    }

    pub fn writable_dirs_mut(&mut self) -> &mut Option<Vec<PathBuf>> {
        &mut self.writable_dirs
    }

    /// Returns the custom path resolver, if any.
    pub fn resolver(&self) -> Option<&PathResolver> {
        self.resolver.as_ref()
    }

    /// Returns a mutable reference to the custom path resolver.
    pub fn resolver_mut(&mut self) -> &mut Option<PathResolver> {
        &mut self.resolver
    }

    /// Set a custom [`PathResolver`] (builder style).
    #[must_use]
    pub fn with_resolver(mut self, resolver: PathResolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Resolve a caller-supplied path to an absolute, logically-normalised
    /// physical path.
    ///
    /// When a custom [`PathResolver`] is configured it runs **first** on the
    /// raw input (the input may be a virtual, non-filesystem path, so `cwd`
    /// is intentionally NOT applied); otherwise relative paths are joined
    /// onto `cwd`.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        let path = if let Some(resolve) = &self.resolver {
            resolve(path)?
        } else {
            path.to_path_buf()
        };
        let path = if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        };
        Ok(sandbox::normalize_path(&path))
    }

    // ── Path resolution ─────────────────────────────────────

    /// Resolve and validate that `path` stays within the readable roots.
    ///
    /// Returns the resolved (symlink-followed) path on success.  Useful for
    /// callers that want a pre-flight check without opening the file.
    pub fn check_readable(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let real = sandbox::resolve_real_path(&self.resolve(path)?);
        sandbox::check_path_in_roots(&real, self.readable_dirs())?;
        Ok(real)
    }

    /// Resolve and validate that `path` stays within the writable roots.
    ///
    /// Returns the resolved (symlink-followed) path on success.  Useful for
    /// callers that want a pre-flight check without opening the file.
    pub fn check_writable(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let real = sandbox::resolve_real_path(&self.resolve(path)?);
        sandbox::check_path_in_roots(&real, self.writable_dirs())?;
        Ok(real)
    }

    /// Validate that a possibly-relative **directory** path stays within the
    /// writable roots.
    pub fn check_dir_writable(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let real = self.check_writable(path)?;

        // If the path already exists it must be a directory.
        if real.exists() && !real.is_dir() {
            tracing::error!(path = %real.display(), "check_dir_writable: path exists but is not a directory");
            return Err(Error::Validation(format!(
                "无效目录路径: {}",
                real.display(),
            )));
        }

        Ok(real)
    }

    // ── File creation / opening ─────────────────────────────

    /// Resolve + TOCTOU-safe **create** a new file with `0o600` permissions.
    ///
    /// The path is first resolved and security-checked, then atomically
    /// created via `create_new(true)`.  Returns the resolved path together
    /// with the opened [`File`].
    pub async fn create_file(&self, path: impl AsRef<Path>) -> Result<(PathBuf, fs::File)> {
        let abs = self.resolve(path)?;
        let dirs = self.writable_dirs.clone();
        spawn_blocking_in_span(move || sandbox::create_file(abs, dirs.as_deref()))
            .await
            .map_err(|e| Error::Other(format!("create_file task panicked: {e}").into()))?
    }

    /// TOCTOU-safe create with unique-suffix collision avoidance.
    ///
    /// Tries [`create_file`](Self::create_file) on `path` first.  If the
    /// file already exists, inserts a random alphanumeric suffix before the
    /// extension and retries (up to 1 000 attempts).
    pub async fn create_file_unique(&self, path: impl AsRef<Path>) -> Result<(PathBuf, fs::File)> {
        let path = self.resolve(path)?;
        let dirs = self.writable_dirs.clone();

        spawn_blocking_in_span(move || {
            let dirs = dirs.as_deref();
            let parent = path.parent().unwrap_or(&path);
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let ext = path.extension().map(|e| e.to_string_lossy());

            let mut candidate = path.clone();
            let mut attempts = 0u32;

            loop {
                match sandbox::create_file(&candidate, dirs) {
                    Ok(pair) => break Ok(pair),
                    Err(Error::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::AlreadyExists
                            && attempts < 1000 =>
                    {
                        let suffix = util::random_str(8);
                        let name = match &ext {
                            Some(ext) => format!("{stem}.{suffix}.{ext}"),
                            None => format!("{stem}.{suffix}"),
                        };
                        candidate = parent.join(name);
                        attempts += 1;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, path = %candidate.display(), "create_file_unique failed");
                        break Err(e);
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Other(format!("create_file_unique task panicked: {e}").into()))?
    }

    // ── Read operations ─────────────────────────────────────

    /// Read a file's entire contents as a UTF-8 string.
    ///
    /// The path is resolved and validated against the readable roots.  After
    /// opening the file, an fd-level TOCTOU verification ensures the file
    /// descriptor actually resides within the allowed roots (guards against
    /// symlink-swap races).  The contents are then read via the open fd,
    /// **not** by re-opening the path.
    pub async fn read_to_string(&self, path: impl AsRef<Path>) -> Result<String> {
        let abs = self.resolve(path)?;
        let dirs = self.readable_dirs.clone();
        spawn_blocking_in_span(move || {
            let (real, mut file) = sandbox::open_file(abs, dirs.as_deref())?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|e| Error::io(format!("Failed to read {}", real.display()), e))?;
            Ok(contents)
        })
        .await
        .map_err(|e| Error::Other(format!("read_to_string task panicked: {e}").into()))?
    }

    /// Get file metadata (size, modified time, etc.).
    ///
    /// The path is resolved and validated against the readable roots.  After
    /// opening the file, an fd-level TOCTOU verification ensures the file
    /// descriptor actually resides within the allowed roots.  Metadata is
    /// then obtained from the open fd, **not** by re-statting the path.
    pub async fn metadata(&self, path: impl AsRef<Path>) -> Result<fs::Metadata> {
        let abs = self.resolve(path)?;
        let dirs = self.readable_dirs.clone();
        spawn_blocking_in_span(move || {
            let (real, file) = sandbox::open_file(abs, dirs.as_deref())?;
            file.metadata()
                .map_err(|e| Error::io(format!("Failed to stat {}", real.display()), e))
        })
        .await
        .map_err(|e| Error::Other(format!("metadata task panicked: {e}").into()))?
    }

    /// Open `path` for streaming reads, returning the resolved real path and
    /// a [`tokio::fs::File`] handle wrapping the sandbox-validated fd.
    ///
    /// This is the streaming counterpart of [`Self::read_to_string`]: callers
    /// who need to read the file **incrementally** (e.g. chunked digest, large
    /// upload) must go through this method rather than calling
    /// `tokio::fs::File::open` directly so that:
    ///
    /// - the path is resolved against `cwd` (relative paths supported),
    /// - the resolved path is validated against the **readable** roots,
    /// - on Linux, an fd-level TOCTOU verification ensures the fd really
    ///   resides under the allowed roots after open.
    ///
    /// The returned `tokio::fs::File` shares the same backing fd; the caller
    /// can drive it with the usual `AsyncReadExt` API. The returned
    /// `PathBuf` is the canonical path that passed the sandbox check.
    pub async fn open_for_read(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(PathBuf, tokio::fs::File)> {
        let abs = self.resolve(path)?;
        let dirs = self.readable_dirs.clone();
        let (real, std_file) =
            spawn_blocking_in_span(move || sandbox::open_file(abs, dirs.as_deref()))
                .await
                .map_err(|e| Error::Other(format!("open_for_read task panicked: {e}").into()))??;
        Ok((real, tokio::fs::File::from_std(std_file)))
    }

    // ── Atomic write ────────────────────────────────────────

    /// Atomically write `data` to `path` (temp file → fsync → rename).
    ///
    /// The target path is first resolved via `cwd` (for relative paths),
    /// then symlinks are followed via `resolve_real_path`, and finally the
    /// result is validated against the allowed writable roots.  Any existing
    /// file at `path` is atomically replaced.
    pub async fn atomic_write(&self, path: &Path, data: &[u8], mode: u32) -> Result<PathBuf> {
        let real = self.check_writable(path)?;

        // Move owned copies into the blocking closure.
        let data = data.to_vec();

        spawn_blocking_in_span(move || sandbox::atomic_write_blocking(&real, &data, mode))
            .await
            .map_err(|e| {
                Error::Other(format!("atomic_write background task panicked: {e}").into())
            })?
    }

    // ── Delete operations ───────────────────────────────────

    /// Remove a single file.
    ///
    /// The path is resolved (symlinks followed) and validated against the
    /// writable roots before deletion.
    pub async fn remove_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let abs = self.resolve(path)?;
        let dirs = self.writable_dirs.clone();
        spawn_blocking_in_span(move || sandbox::sandbox_remove_file(abs, dirs.as_deref()))
            .await
            .map_err(|e| Error::Other(format!("remove_file task panicked: {e}").into()))?
    }

    /// Recursively remove a directory and all its contents.
    ///
    /// The path is resolved (symlinks followed) and validated against the
    /// writable roots before deletion.
    pub async fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
        let abs = self.resolve(path)?;
        let dirs = self.writable_dirs.clone();
        spawn_blocking_in_span(move || sandbox::sandbox_remove_dir_all(abs, dirs.as_deref()))
            .await
            .map_err(|e| Error::Other(format!("remove_dir_all task panicked: {e}").into()))?
    }

    // ── Directory listing ───────────────────────────────────

    /// List all entries (files and subdirectories) in a directory.
    ///
    /// The path is resolved (symlinks followed) and validated against the
    /// readable roots.  Callers filter by type themselves.
    pub async fn list_dir(&self, dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let abs = self.resolve(dir)?;
        let roots = self.readable_dirs.clone();
        spawn_blocking_in_span(move || sandbox::sandbox_list_dir(abs, roots.as_deref()))
            .await
            .map_err(|e| Error::Other(format!("list_dir task panicked: {e}").into()))?
    }

    // ── Fuzzy path correction ─────────────────────────────────

    /// Resolve a readable path, with fuzzy fallback.
    ///
    /// If the path passes `check_readable` **and the file actually exists**, it
    /// is returned as-is.  This resolver serves *read* (upload source) paths,
    /// so a path that merely stays within the roots but does not exist is
    /// **not** returned directly — its tail / filename may carry noise and is
    /// routed through correction below.
    ///
    /// Otherwise correction kicks in: phase 1 locates an anchor directory
    /// under the readable roots (restricted) or the deepest existing ancestor
    /// (unrestricted), and phase 2 walks the remaining tail segments matching
    /// against real filesystem entries per level.
    ///
    /// On anchor failure a `Permission` error is returned (the path could not
    /// be mapped to any readable root — semantically an access denial).  On
    /// correction failure (a level did not pass confidence gates) a uniform
    /// "找不到目标文件" error is returned.
    ///
    /// The per-level matching logic lives in [`fuzzy`] (pure primitives +
    /// [`fuzzy::walk_tail`]); this method is the thin async orchestrator that
    /// resolves `Fs`-level dependencies (`check_readable`, `readable_dirs`)
    /// and dispatches phase 2 in a single `spawn_blocking`.
    pub(crate) async fn resolve_readable_or_suggest(&self, file_path: &str) -> Result<PathBuf> {
        // 1. Direct resolution → return only when the file actually exists.
        //    Capture the original error: if correction also fails to anchor,
        //    surface it rather than masking as "file not found".
        let direct_err: Option<Error> = match self.check_readable(file_path) {
            Ok(real) if real.exists() => return Ok(real),
            Ok(_) => None,
            Err(e) => Some(e),
        };

        let abs = self.resolve(file_path)?;
        let in_comps: Vec<_> = abs.components().collect();

        // 2. Phase 1: locate anchor directory A and tail segments.
        //    On anchor failure, surface a Permission error (the path could
        //    not be mapped to any readable root — semantically an access
        //    denial, not a "file not found").
        let (anchor, tail) = match self.readable_dirs() {
            // restricted: in-memory root-trie (no fs listing above roots)
            Some(roots) => fuzzy::RootTrie::build(roots)
                .anchor(&in_comps)
                .ok_or_else(|| {
                    direct_err.unwrap_or_else(|| {
                        Error::Permission(format!("目标路径超出可访问范围: {file_path}"))
                    })
                })?,
            // unrestricted: deepest existing ancestor as anchor
            None => fuzzy::deepest_existing_ancestor(&sandbox::resolve_real_path(&abs))
                .ok_or_else(|| {
                    direct_err.unwrap_or_else(|| fuzzy::readable_not_found(file_path))
                })?,
        };

        if tail.is_empty() || tail.len() > fuzzy::WALK_DEPTH_MAX {
            // Anchor was found but the tail is empty or too long — this is a
            // correction failure (path genuinely not found), not an anchor
            // failure, so use readable_not_found rather than the original error.
            telemetry::emit(
                ctr::KIND,
                &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_ERR }),
            );
            return Err(fuzzy::readable_not_found(file_path));
        }

        // 3. Phase 2: per-level fuzzy walk in a single blocking task.
        let roots = self.readable_dirs.clone();
        let fp = file_path.to_string();
        let cur =
            spawn_blocking_in_span(move || fuzzy::walk_tail(anchor, &tail, roots.as_deref(), &fp))
                .await
                .map_err(|e| {
                    telemetry::emit(
                        ctr::KIND,
                        &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_ERR }),
                    );
                    Error::Other(format!("fuzzy walk task panicked: {e}").into())
                })??;

        // 4. Final sandbox validation (defensive; cur was walked from a
        //    legitimate anchor, but re-verify for TOCTOU safety).
        tracing::info!(
            to = %cur.display(),
            "path fuzzy-corrected"
        );
        self.check_readable(&cur)
            .inspect(|_| {
                telemetry::emit(
                    ctr::KIND,
                    &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_OK_CORRECTED }),
                );
            })
            .inspect_err(|e| {
                tracing::warn!(
                    error = %e,
                    to = %cur.display(),
                    "corrected path failed final sandbox re-check"
                );
                telemetry::emit(
                    ctr::KIND,
                    &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_ERR }),
                );
            })
    }

    /// Resolve + fuzzy-correct a writable file path.
    ///
    /// Only the **anchor root** is corrected: phase 1 uses writable roots
    /// (`RootTrie`) to find the correct sandbox prefix.  The remaining tail
    /// segments are joined as-is — no per-level fuzzy walk, because the
    /// target file / directory may not exist yet.
    ///
    /// 1. Fast path: `check_writable()` succeeds → return immediately.
    /// 2. Phase 1: anchor with writable root trie (or `deepest_existing_ancestor`).
    /// 3. Join `anchor + tail` verbatim.
    /// 4. Final `check_writable()` for TOCTOU safety.
    ///
    /// On anchor failure, returns `Error::Permission`.
    pub(crate) async fn resolve_writable_or_suggest(
        &self,
        file_path: impl AsRef<Path>,
    ) -> Result<PathBuf> {
        let file_path = file_path.as_ref();

        // Unrestricted mode: no sandbox to correct, just validate.
        let Some(roots) = self.writable_dirs() else {
            return self.check_writable(file_path);
        };

        // 1. Fast path: directly writable → return immediately.
        if let Ok(real) = self.check_writable(file_path) {
            return Ok(real);
        }

        let abs = self.resolve(file_path)?;
        let in_comps: Vec<_> = abs.components().collect();
        let original = file_path.display().to_string();

        // 2. Phase 1: anchor with writable root trie (root correction only).
        let (anchor, tail) = fuzzy::RootTrie::build(roots)
            .anchor(&in_comps)
            .ok_or_else(|| Error::Permission(format!("目标路径超出可访问范围: {}", original)))?;

        // 3. Join anchor + tail verbatim (no per-level fuzzy walk).
        let cur: PathBuf = tail.iter().fold(anchor, |p, seg| p.join(seg));

        // 4. Final sandbox validation.
        tracing::info!(
            to = %cur.display(),
            "path fuzzy-corrected"
        );
        self.check_writable(&cur)
            .inspect(|_| {
                telemetry::emit(
                    ctr::KIND,
                    &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_OK_CORRECTED }),
                );
            })
            .inspect_err(|e| {
                tracing::warn!(
                    error = %e,
                    to = %cur.display(),
                    "corrected writable_path failed final sandbox re-check"
                );
                telemetry::emit(
                    ctr::KIND,
                    &serde_json::json!({ ctr::FIELD_OUTCOME: ctr::OUTCOME_ERR }),
                );
            })
    }

    /// Like [`resolve_writable_or_suggest`], but additionally requires the
    /// target to be a directory (or not exist yet; existing files are rejected).
    pub(crate) async fn resolve_dir_writable_or_suggest(
        &self,
        dir_path: impl AsRef<Path>,
    ) -> Result<PathBuf> {
        let resolved = self.resolve_writable_or_suggest(&dir_path).await?;

        // If the path already exists it must be a directory.
        if resolved.exists() && !resolved.is_dir() {
            return Err(Error::Validation(format!(
                "无效目录路径: {}",
                resolved.display(),
            )));
        }

        Ok(resolved)
    }
}

/// Like `tokio::task::spawn_blocking`, but inherits the current tracing span.
///
/// `spawn_blocking` runs the closure on a separate thread where the
/// thread-local span context is not propagated.  This helper captures the
/// current span before spawning and enters it inside the closure so that
/// logs / structured events emitted from the blocking task stay under the
/// caller's span.
fn spawn_blocking_in_span<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let span = tracing::Span::current();
    spawn_blocking(move || {
        let _enter = span.enter();
        f()
    })
}

// ── Standalone helpers (no Fs state needed) ─────────────────

/// Sanitize a filename for safe use on all platforms.
pub fn sanitize_filename(name: &str) -> String {
    sanitize::sanitize_filename(name, cfg!(windows))
}

/// Pre-flight check: verify that the file at `file_path` does not exceed
/// `max_size` bytes, so callers can fail fast on oversized files without
/// wasting network I/O.
pub async fn check_file_size_limit(fs: &Fs, file_path: &str, max_size: u64) -> crate::Result<()> {
    let file_size = fs.metadata(file_path).await?.len();

    if file_size > max_size {
        tracing::warn!(file_size, limit = max_size, "file exceeds size limit");
        return Err(crate::Error::Validation(format!(
            "文件 \"{file_path}\" 大小超过 {:.1} MB 限制",
            max_size as f64 / 1_048_576.0,
        )));
    }

    Ok(())
}

pub use reqwest_ext::{content_disposition_filename, stream_to_file};

// ══════════════════════════════════════════════════════════════
//  Tests — Fs integration tests (atomic_write w/ roots)
// ══════════════════════════════════════════════════════════════

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：Fs（文件系统抽象层）
    //!
    //! ### 关键接口
    //! - [Fs::atomic_write] — 原子写入：先写临时文件，再 rename 到目标路径
    //! - [Fs::create_file] — 在沙箱内创建文件，返回 tokio File handle
    //! - [Fs::read_to_string] — 读取文件为 UTF-8 字符串，带沙箱校验
    //! - [Fs::remove_file] / [Fs::remove_dir_all] — 沙箱内删除
    //! - [Fs::list_dir] — 列出目录下所有条目（非递归，含文件和子目录）
    //! - [Fs::create_file_unique] — 创建带唯一后缀的文件（冲突时自动重试）
    //! - [Fs::metadata] — 获取文件元信息（size 等），fd 级 TOCTOU 安全
    //! - [Fs::check_readable] / [Fs::check_writable] — 校验路径是否在允许的 roots 内
    //! - [Fs::cwd] — 返回当前工作目录
    //! - [Fs::resolve] — 解析路径为绝对物理路径（支持自定义 [PathResolver]）
    //! - [sanitize_filename] — 清理文件名中的非法字符
    //!
    //! ### 关键分支与异常路径
    //! - 路径逃逸 writable/readable roots → 返回 Err("escapes allowed directories")
    //! - 相对路径 → 自动拼接 cwd 后再校验
    //! - 目标路径是文件而非目录 → Err("Expected a directory") (check_dir_writable)
    //! - 已存在文件 create_file → Err(AlreadyExists)
    //! - create_file_unique 冲突 → 自动追加随机后缀重试
    //! - None roots（无限制模式）→ 跳过所有校验
    //! - 分离的 readable/writable roots → 读写操作分别校验不同 root 列表
    //!
    //! ### 上下游交互
    //! - 上游：[directive::file_save]、[directive::octet_stream]、`service/output`、`registry/cache` 通过 [Fs] 实例操作文件
    //! - 下游：所有 I/O 委托给 `sandbox.rs`（路径解析 + roots 校验 + 实际系统调用）

    use std::fs as stdfs;

    use tempfile::TempDir;

    use super::*;

    // ── atomic_write ──

    /// P1：拒绝写入 roots 之外的路径 [[Fs::atomic_write]]
    /// 条件：目标文件位于 forbidden 目录（不在 writable roots 内）
    /// 断言：返回 Err，错误信息包含 "escapes allowed directories"，目标文件未被创建
    #[tokio::test]
    async fn atomic_write_rejects_path_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let target = forbidden.path().join("escaped.txt");

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.atomic_write(&target, b"bad", 0o600).await;

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
        assert!(!target.exists());
    }

    /// P0：允许写入 roots 内的路径 [[Fs::atomic_write]]
    /// 条件：目标文件位于 writable roots 内
    /// 断言：写入成功，文件内容为 "ok"
    #[tokio::test]
    async fn atomic_write_allows_path_within_roots() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("allowed.txt");

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.atomic_write(&target, b"ok", 0o600).await;

        assert!(result.is_ok());
        assert_eq!(stdfs::read_to_string(&target).unwrap(), "ok");
    }

    /// P2：相对路径自动拼接 cwd 后写入 [[Fs::atomic_write]]
    /// 条件：传入相对路径 "rel.txt"
    /// 断言：写入成功，cwd 下文件内容为 "data"
    #[tokio::test]
    async fn atomic_write_relative_path() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.atomic_write(Path::new("rel.txt"), b"data", 0o600).await;
        assert!(result.is_ok());
        assert_eq!(
            stdfs::read_to_string(tmp.path().join("rel.txt")).unwrap(),
            "data"
        );
    }

    // ── check_dir_writable ──

    /// P0：已存在的目录通过校验，返回 canonicalize 后的路径 [[Fs::check_dir_writable]]
    /// 条件：路径是 roots 内已存在的子目录
    /// 断言：返回 Ok，结果与 canonicalize 一致（macOS 上可能解析符号链接如 /var → /private/var）
    #[test]
    fn check_dir_writable_existing_directory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        stdfs::create_dir(&sub).unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.check_dir_writable(&sub);
        assert!(result.is_ok());
        // On macOS, canonicalize may resolve symlinks (e.g. /var → /private/var),
        // so compare against the canonicalized path.
        let expected = sub.canonicalize().unwrap_or(sub);
        assert_eq!(result.unwrap(), expected);
    }

    /// P1：不存在的路径也通过校验（由调用者后续创建）[[Fs::check_dir_writable]]
    /// 条件：路径在 roots 内但不存在
    /// 断言：返回 Ok
    #[test]
    fn check_dir_writable_nonexistent_is_ok() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        // Non-existent path within root is fine (caller creates it later).
        let result = fs.check_dir_writable(tmp.path().join("future-dir"));
        assert!(result.is_ok());
    }

    /// P1：路径是普通文件而非目录时报错 [[Fs::check_dir_writable]]
    /// 条件：路径存在且是普通文件
    /// 断言：返回 Err，错误信息包含 "无效目录路径"
    #[test]
    fn check_dir_writable_rejects_file_as_dir() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("not-a-dir");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.check_dir_writable(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("无效目录路径"));
    }

    /// P1：路径在 roots 之外时报错 [[Fs::check_dir_writable]]
    /// 条件：路径在 forbidden 目录
    /// 断言：返回 Err
    #[test]
    fn check_dir_writable_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.check_dir_writable(forbidden.path());
        assert!(result.is_err());
    }

    /// P1：相对路径正确解析 [[Fs::check_dir_writable]]
    /// 条件：传入相对路径 "sub"，子目录已存在
    /// 断言：返回 Ok
    #[test]
    fn check_dir_writable_relative_path() {
        let tmp = TempDir::new().unwrap();
        stdfs::create_dir(tmp.path().join("sub")).unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.check_dir_writable("sub");
        assert!(result.is_ok());
    }

    // ── create_file ──

    /// P0：正常创建文件 [[Fs::create_file]]
    /// 条件：路径在 roots 内，目标文件不存在
    /// 断言：文件存在，文件名为 "output.txt"
    #[tokio::test]
    async fn create_file_success() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _file) = fs.create_file("output.txt").await.unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "output.txt");
    }

    /// P0：自动创建多级父目录 [[Fs::create_file]]
    /// 条件：路径含深层嵌套 "deep/nested/file.txt"
    /// 断言：文件创建成功且存在
    #[tokio::test]
    async fn create_file_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _file) = fs.create_file("deep/nested/file.txt").await.unwrap();
        assert!(path.exists());
    }

    /// P1：拒绝在 roots 之外创建文件
    /// 条件：目标路径在 forbidden 目录
    /// 断言：返回 Err
    #[tokio::test]
    async fn create_file_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.create_file(forbidden.path().join("escape.txt")).await;
        assert!(result.is_err());
    }

    /// P1：目标文件已存在时返回错误（create_new 语义）
    /// 条件：目标文件已被预先创建
    /// 断言：返回 Err
    #[tokio::test]
    async fn create_file_already_exists_returns_err() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("exists.txt"), "data").unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.create_file("exists.txt").await;
        assert!(result.is_err());
    }

    // ── create_file_unique ──

    /// P0：无冲突时使用原名创建
    /// 条件：目标文件不存在
    /// 断言：文件存在，文件名为 "report.json"
    #[tokio::test]
    async fn create_file_unique_no_collision() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _) = fs.create_file_unique("report.json").await.unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "report.json");
    }

    /// P0：有冲突时自动加随机后缀（保留原始扩展名）
    /// 条件：同名文件 "report.json" 已存在
    /// 断言：新文件名 ≠ "report.json"，但包含 "report." 和 ".json"
    #[tokio::test]
    async fn create_file_unique_with_collision() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("report.json"), "existing").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _) = fs.create_file_unique("report.json").await.unwrap();
        assert!(path.exists());
        assert_ne!(path.file_name().unwrap(), "report.json");
        assert!(path.to_string_lossy().contains("report."));
        assert!(path.to_string_lossy().contains(".json"));
    }

    /// P1：无扩展名文件冲突时加随机后缀，不产生多余点号
    /// 条件：同名无扩展名文件 "noext" 已存在
    /// 断言：新文件名以 "noext." 开头且不含第二个点
    #[tokio::test]
    async fn create_file_unique_no_extension_with_collision() {
        let tmp = TempDir::new().unwrap();
        // Pre-create file without extension to trigger `None` ext branch
        stdfs::write(tmp.path().join("noext"), "existing").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _) = fs.create_file_unique("noext").await.unwrap();
        assert!(path.exists());
        assert_ne!(path.file_name().unwrap(), "noext");
        // Should be "noext.<random>" with no trailing extension
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("noext."), "name = {name}");
        // Should NOT have a second dot (no extension was added)
        let after_stem = name.strip_prefix("noext.").unwrap();
        assert!(
            !after_stem.contains('.'),
            "no-extension file should not have extra dot: {name}"
        );
    }

    /// P1：roots 之外拒绝创建
    /// 条件：目标路径在 forbidden 目录
    /// 断言：返回 Err
    #[tokio::test]
    async fn create_file_unique_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs
            .create_file_unique(forbidden.path().join("escape.txt"))
            .await;
        assert!(result.is_err());
    }

    // ── read_to_string ──

    /// P0：正常读取文件内容
    /// 条件：文件存在且在 readable roots 内
    /// 断言：返回 "hello world"
    #[tokio::test]
    async fn read_to_string_success() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("data.txt");
        stdfs::write(&file, "hello world").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello world");
    }

    /// P1：拒绝读取 roots 之外的文件
    /// 条件：文件在 forbidden 目录
    /// 断言：返回 Err
    #[tokio::test]
    async fn read_to_string_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let file = forbidden.path().join("secret.txt");
        stdfs::write(&file, "secret").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.read_to_string(&file).await.is_err());
    }

    /// P1：读取不存在的文件返回 I/O 错误 [[Fs::read_to_string]]
    /// 条件：文件不存在但路径在 roots 内
    /// 断言：返回 Err，错误信息包含 "Failed to open"
    #[tokio::test]
    async fn read_to_string_nonexistent_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.read_to_string(tmp.path().join("missing.txt")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to open"));
    }

    // ── metadata ──

    /// P0：正常获取文件元信息
    /// 条件：5 字节文件在 roots 内
    /// 断言：len() == 5，is_file() == true
    #[tokio::test]
    async fn metadata_returns_file_info() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.bin");
        stdfs::write(&file, "12345").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let meta = fs.metadata(&file).await.unwrap();
        assert_eq!(meta.len(), 5);
        assert!(meta.is_file());
    }

    /// P1：[Fs::metadata] 拒绝获取 roots 之外的文件元信息
    /// 条件：文件在 forbidden 目录
    /// 断言：返回 Err
    #[tokio::test]
    async fn metadata_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let file = forbidden.path().join("secret.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.metadata(&file).await.is_err());
    }

    /// P1：[Fs::metadata] 不存在的文件返回 I/O 错误
    /// 条件：文件不存在但路径在 roots 内
    /// 断言：返回 Err，错误信息包含 "Failed to stat" 或 "Failed to open"
    #[tokio::test]
    async fn metadata_nonexistent_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.metadata(tmp.path().join("missing")).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Failed to stat") || msg.contains("Failed to open"),
            "msg = {msg}"
        );
    }

    // ── remove_file ──

    /// P0：[Fs::remove_file] 正常删除文件
    /// 条件：文件存在且在 writable roots 内
    /// 断言：文件不再存在
    #[tokio::test]
    async fn remove_file_success() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("to-delete.txt");
        stdfs::write(&file, "bye").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        fs.remove_file(&file).await.unwrap();
        assert!(!file.exists());
    }

    /// P1：[Fs::remove_file] 拒绝删除 roots 之外的文件
    /// 条件：文件在 forbidden 目录
    /// 断言：返回 Err，文件仍存在
    #[tokio::test]
    async fn remove_file_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let file = forbidden.path().join("no-delete.txt");
        stdfs::write(&file, "keep").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.remove_file(&file).await.is_err());
        assert!(file.exists());
    }

    /// P1：[Fs::remove_file] 删除不存在的文件返回 I/O 错误
    /// 条件：文件不存在但路径在 roots 内
    /// 断言：返回 Err，错误信息包含 "Failed to remove"
    #[tokio::test]
    async fn remove_file_nonexistent_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.remove_file(tmp.path().join("gone.txt")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to remove"));
    }

    // ── remove_dir_all ──

    /// P0：[Fs::remove_dir_all] 正常递归删除目录（含子文件）
    /// 条件：含子目录和文件的目录在 writable roots 内
    /// 断言：目录不再存在
    #[tokio::test]
    async fn remove_dir_all_success() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("mydir");
        stdfs::create_dir_all(dir.join("sub")).unwrap();
        stdfs::write(dir.join("sub/file.txt"), "data").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        fs.remove_dir_all(&dir).await.unwrap();
        assert!(!dir.exists());
    }

    /// P1：[Fs::remove_dir_all] 拒绝删除 roots 之外的目录
    /// 条件：目录在 forbidden 目录
    /// 断言：返回 Err，目录仍存在
    #[tokio::test]
    async fn remove_dir_all_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let dir = forbidden.path().join("keep-dir");
        stdfs::create_dir(&dir).unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.remove_dir_all(&dir).await.is_err());
        assert!(dir.exists());
    }

    /// P1：[Fs::remove_dir_all] 删除不存在的目录返回 I/O 错误
    /// 条件：目录不存在但路径在 roots 内
    /// 断言：返回 Err，错误信息包含 "Failed to remove"
    #[tokio::test]
    async fn remove_dir_all_nonexistent_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.remove_dir_all(tmp.path().join("gone-dir")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to remove"));
    }

    // ── list_dir ──

    /// P0：[Fs::list_dir] 返回所有条目，调用方可过滤为仅文件
    /// 条件：目录下有 2 个文件和 1 个子目录
    /// 断言：list_dir 返回 3 个条目；过滤 is_file() 后得 2 个
    #[tokio::test]
    async fn list_dir_returns_all_entries() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("a.txt"), "a").unwrap();
        stdfs::write(tmp.path().join("b.txt"), "b").unwrap();
        stdfs::create_dir(tmp.path().join("subdir")).unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let entries = fs.list_dir(tmp.path()).await.unwrap();
        // list_dir returns ALL entries (files + dirs), no filtering.
        assert_eq!(entries.len(), 3);
        let file_count = entries.iter().filter(|p| p.is_file()).count();
        assert_eq!(file_count, 2);
        let dir_count = entries.iter().filter(|p| p.is_dir()).count();
        assert_eq!(dir_count, 1, "list_dir must return subdirectory entries");
    }

    /// P1：空目录返回空列表 [[Fs::list_dir]]
    /// 条件：目录存在但为空
    /// 断言：entries.is_empty()
    #[tokio::test]
    async fn list_dir_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let entries = fs.list_dir(tmp.path()).await.unwrap();
        assert!(entries.is_empty());
    }

    /// P1：拒绝列出 roots 之外的目录 [[Fs::list_dir]]
    /// 条件：目录在 forbidden 目录
    /// 断言：返回 Err
    #[tokio::test]
    async fn list_dir_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.list_dir(forbidden.path()).await.is_err());
    }

    /// P1：不存在的目录返回 I/O 错误 [[Fs::list_dir]]
    /// 条件：目录不存在但路径在 roots 内
    /// 断言：返回 Err，错误信息包含 "Failed to read directory"
    #[tokio::test]
    async fn list_dir_nonexistent_dir_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.list_dir(tmp.path().join("no-dir")).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read directory")
        );
    }

    // ── sanitize_filename ──

    /// P0：[sanitize_filename] 正常文件名保持不变
    /// 条件：输入 "report.pdf"
    /// 断言：输出等于 "report.pdf"
    #[test]
    fn sanitize_filename_normal() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
    }

    /// P1：[sanitize_filename] 含路径分隔符的文件名被清理（去除斜杠）
    /// 条件：输入 "path/to/file.txt"
    /// 断言：结果不含 '/'
    #[test]
    fn sanitize_filename_with_slashes() {
        let result = sanitize_filename("path/to/file.txt");
        assert!(!result.contains('/'));
    }

    // ── resolve (relative vs absolute) ──

    /// P0：相对路径拼接到 cwd 得到绝对路径 [[Fs::resolve]]
    /// 条件：cwd = "/project"，输入 "sub/file.txt"
    /// 断言：输出 "/project/sub/file.txt"
    #[test]
    fn resolve_resolves_relative_path() {
        let fs = Fs::new_with_permissions(
            "/project",
            Some(&[Path::new("/project")]),
            Some(&[Path::new("/project")]),
        );
        assert_eq!(
            fs.resolve("sub/file.txt").unwrap(),
            PathBuf::from("/project/sub/file.txt")
        );
    }

    /// P0：绝对路径原样返回 [[Fs::resolve]]
    /// 条件：输入 "/other/file.txt"
    /// 断言：输出等于输入
    #[test]
    fn resolve_keeps_absolute_path() {
        let fs = Fs::new_with_permissions(
            "/project",
            Some(&[Path::new("/project")]),
            Some(&[Path::new("/project")]),
        );
        assert_eq!(
            fs.resolve("/other/file.txt").unwrap(),
            PathBuf::from("/other/file.txt")
        );
    }

    // ── resolve with resolver ──

    /// P0：自定义 resolver 将虚拟路径映射到物理路径 [[Fs::resolve]] [[PathResolver]]
    /// 条件：cwd = "/project"，resolver 将 "virtual://ws/foo.txt" 映射到 "/data/roots/ws/foo.txt"
    /// 断言：输出 "/data/roots/ws/foo.txt"
    #[test]
    fn resolve_with_custom_resolver_maps_virtual_path() {
        let resolver: PathResolver = Arc::new(|p: &Path| {
            let s = p.to_string_lossy();
            if let Some(rest) = s.strip_prefix("virtual://ws/") {
                Ok(PathBuf::from("/data/roots/ws").join(rest))
            } else {
                Ok(p.to_path_buf())
            }
        });
        let fs = Fs::new("/project").with_resolver(resolver);
        assert_eq!(
            fs.resolve("virtual://ws/foo.txt").unwrap(),
            PathBuf::from("/data/roots/ws/foo.txt")
        );
    }

    /// P1：resolver 优先于 cwd [[Fs::resolve]]
    /// 条件：cwd = "/project"，resolver 映射 "virtual://..." → "/data/..."
    /// 断言：cwd 不参与 resolver 分支的路径拼接
    #[test]
    fn resolve_resolver_takes_precedence_over_cwd() {
        let resolver: PathResolver = Arc::new(|p: &Path| {
            if p.to_string_lossy().starts_with("virtual://") {
                Ok(PathBuf::from("/data/roots").join(p.strip_prefix("virtual://").unwrap()))
            } else {
                Ok(p.to_path_buf())
            }
        });
        let fs = Fs::new("/project").with_resolver(resolver);
        // 即使 cwd 是 /project，虚拟路径也不拼接 cwd
        assert_eq!(
            fs.resolve("virtual://ws/file.txt").unwrap(),
            PathBuf::from("/data/roots/ws/file.txt")
        );
    }

    /// P1：无 resolver 时相对路径仍按 cwd 拼接 [[Fs::resolve]]
    /// 条件：无 resolver，输入相对路径 "sub/file.txt"
    /// 断言：输出为 cwd.join("sub/file.txt") 的规范化结果
    #[test]
    fn resolve_without_resolver_uses_cwd_for_relative() {
        let fs = Fs::new("/project");
        assert_eq!(
            fs.resolve("sub/file.txt").unwrap(),
            PathBuf::from("/project/sub/file.txt")
        );
    }

    /// P1：无 resolver 时绝对路径原样返回 [[Fs::resolve]]
    /// 条件：无 resolver，输入绝对路径 "/abs/path"
    /// 断言：输出等于输入（经 normalize 后）
    #[test]
    fn resolve_without_resolver_keeps_absolute() {
        let fs = Fs::new("/project");
        assert_eq!(fs.resolve("/abs/path").unwrap(), PathBuf::from("/abs/path"));
    }

    /// P1：resolver 返回的路径也经过 normalize [[Fs::resolve]]
    /// 条件：resolver 返回包含 "/./" 和 "/../" 的路径
    /// 断言：输出已剥离 "." 和 ".."
    #[test]
    fn resolve_normalizes_resolver_output() {
        let resolver: PathResolver =
            Arc::new(|_p: &Path| Ok(PathBuf::from("/data/./roots/../roots/ws")));
        let fs = Fs::new("/project").with_resolver(resolver);
        assert_eq!(
            fs.resolve("any-input").unwrap(),
            PathBuf::from("/data/roots/ws")
        );
    }

    /// P1：resolver 返回相对路径时回退到 cwd 补齐 [[Fs::resolve]]
    /// 条件：resolver 违约返回相对路径 "sub/file.txt"，cwd = "/project"
    /// 断言：输出 "/project/sub/file.txt"（release 下 debug_assert 为 no-op 时兜底）
    #[test]
    fn resolve_falls_back_to_cwd_when_resolver_returns_relative() {
        let resolver: PathResolver = Arc::new(|_p: &Path| Ok(PathBuf::from("sub/file.txt")));
        let fs = Fs::new("/project").with_resolver(resolver);
        assert_eq!(
            fs.resolve("any-input").unwrap(),
            PathBuf::from("/project/sub/file.txt")
        );
    }

    /// P1：resolver 返回 Err 时 resolve 传播错误 [[Fs::resolve]]
    /// 条件：resolver 对特定输入返回 Error::Validation
    /// 断言：resolve 返回相同错误
    #[test]
    fn resolve_propagates_resolver_error() {
        let resolver: PathResolver =
            Arc::new(|p: &Path| Err(Error::Validation(format!("无法映射: {}", p.display()))));
        let fs = Fs::new("/project").with_resolver(resolver);
        let err = fs.resolve("bad-input").unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        assert!(err.to_string().contains("无法映射"));
    }

    /// P2：resolver 可处理非路径 scheme 输入 [[Fs::resolve]]
    /// 条件：resolver 解析 "db://table/row" → "/mnt/db/table/row"
    /// 断言：正确映射
    #[test]
    fn resolve_handles_non_filesystem_scheme() {
        let resolver: PathResolver = Arc::new(|p: &Path| {
            let s = p.to_string_lossy();
            if let Some(rest) = s.strip_prefix("db://") {
                Ok(PathBuf::from("/mnt/db").join(rest))
            } else {
                Err(Error::Validation(format!("未知 scheme: {s}")))
            }
        });
        let fs = Fs::new("/project").with_resolver(resolver);
        assert_eq!(
            fs.resolve("db://table/row").unwrap(),
            PathBuf::from("/mnt/db/table/row")
        );
    }

    // ══════════════════════════════════════════════════════════════
    //  Tests — new_with_permissions (separate read/write roots)
    // ══════════════════════════════════════════════════════════════

    /// P0：只读 root 允许读取文件 [[Fs::read_to_string]]
    /// 条件：readable 含 read_dir，writable 不含
    /// 断言：读取成功返回 "hello"
    #[tokio::test]
    async fn read_only_root_allows_read_to_string() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let file = read_dir.path().join("data.txt");
        stdfs::write(&file, "hello").unwrap();

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable
        );
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello");
    }

    /// P1：只读 root 阻止创建文件 [[Fs::create_file]]
    /// 条件：writable 不含 read_dir
    /// 断言：返回 Err，错误信息包含 "目标路径超出可访问范围"
    #[tokio::test]
    async fn read_only_root_blocks_create_file() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable — does NOT include read_dir
        );

        // Writing inside the read-only dir should fail.
        let result = fs.create_file(read_dir.path().join("forbidden.txt")).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
    }

    /// P0：只写 root 允许创建文件 [[Fs::create_file]]
    /// 条件：writable 含 write_dir，readable 不含
    /// 断言：创建成功返回 Ok
    #[tokio::test]
    async fn write_only_root_allows_create_file() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            write_dir.path(),
            Some(&[read_dir.path()]), // readable — does NOT include write_dir
            Some(&[write_dir.path()]), // writable
        );

        let result = fs.create_file(write_dir.path().join("ok.txt")).await;
        assert!(result.is_ok());
    }

    /// P1：只写 root 阻止读取文件 [[Fs::read_to_string]]
    /// 条件：readable 不含 write_dir，文件已存在
    /// 断言：返回 Err，错误信息包含 "目标路径超出可访问范围"
    #[tokio::test]
    async fn write_only_root_blocks_read_to_string() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let file = write_dir.path().join("secret.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(
            write_dir.path(),
            Some(&[read_dir.path()]), // readable — does NOT include write_dir
            Some(&[write_dir.path()]), // writable
        );

        let result = fs.read_to_string(&file).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
    }

    /// P1：只写 root 阻止获取元数据 [[Fs::metadata]]
    /// 条件：readable 不含 write_dir
    /// 断言：返回 Err
    #[tokio::test]
    async fn write_only_root_blocks_metadata() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let file = write_dir.path().join("meta.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(
            write_dir.path(),
            Some(&[read_dir.path()]),  // readable only
            Some(&[write_dir.path()]), // writable
        );

        assert!(fs.metadata(&file).await.is_err());
    }

    /// P1：只写 root 阻止列出目录 [[Fs::list_dir]]
    /// 条件：readable 不含 write_dir
    /// 断言：返回 Err
    #[tokio::test]
    async fn write_only_root_blocks_list_dir() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        stdfs::write(write_dir.path().join("a.txt"), "a").unwrap();

        let fs = Fs::new_with_permissions(
            write_dir.path(),
            Some(&[read_dir.path()]),  // readable only
            Some(&[write_dir.path()]), // writable
        );

        assert!(fs.list_dir(write_dir.path()).await.is_err());
    }

    /// P1：只读 root 阻止删除文件 [[Fs::remove_file]]
    /// 条件：writable 不含 read_dir，文件已存在
    /// 断言：返回 Err 且文件仍存在
    #[tokio::test]
    async fn read_only_root_blocks_remove_file() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let file = read_dir.path().join("protected.txt");
        stdfs::write(&file, "keep").unwrap();

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable — does NOT include read_dir
        );

        let result = fs.remove_file(&file).await;
        assert!(result.is_err());
        // File should still exist — delete was blocked.
        assert!(file.exists());
    }

    /// P1：只读 root 阻止递归删除目录 [[Fs::remove_dir_all]]
    /// 条件：writable 不含 read_dir，子目录已存在
    /// 断言：返回 Err 且目录仍存在
    #[tokio::test]
    async fn read_only_root_blocks_remove_dir_all() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let sub = read_dir.path().join("subdir");
        stdfs::create_dir(&sub).unwrap();

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable
        );

        assert!(fs.remove_dir_all(&sub).await.is_err());
        assert!(sub.exists());
    }

    /// P1：只读 root 阻止原子写入
    /// 条件：writable 不含 read_dir
    /// 断言：返回 Err 且目标文件未创建
    #[tokio::test]
    async fn read_only_root_blocks_atomic_write() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();
        let target = read_dir.path().join("no-write.txt");

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable
        );

        let result = fs.atomic_write(&target, b"bad", 0o600).await;
        assert!(result.is_err());
        assert!(!target.exists());
    }

    /// P1：可写 root 允许原子写入 [[Fs::atomic_write]]
    /// 条件：readable + writable 均含 write_dir
    /// 断言：写入成功，内容为 "data"
    #[tokio::test]
    async fn writable_root_allows_atomic_write() {
        let write_dir = TempDir::new().unwrap();
        let target = write_dir.path().join("ok.txt");

        let fs = Fs::new_with_permissions(
            write_dir.path(),
            Some(&[write_dir.path()]), // readable
            Some(&[write_dir.path()]), // writable
        );

        let result = fs.atomic_write(&target, b"data", 0o600).await;
        assert!(result.is_ok());
        assert_eq!(stdfs::read_to_string(&target).unwrap(), "data");
    }

    /// P0：同时在读写 roots 内可完整读写 [[Fs::new_with_permissions]]
    /// 条件：同一目录同时在 readable 和 writable 列表中
    /// 断言：读和写操作均成功
    #[tokio::test]
    async fn both_roots_allow_full_access() {
        let dir = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(
            dir.path(),
            Some(&[dir.path()]), // readable + writable
            Some(&[dir.path()]),
        );

        // Read
        let file = dir.path().join("rw.txt");
        stdfs::write(&file, "hello").unwrap();
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello");

        // Write
        let new_file = dir.path().join("created.txt");
        let (path, _) = fs.create_file(&new_file).await.unwrap();
        assert!(path.exists());
    }

    /// P1：check_dir_writable 使用 writable roots 校验（非 readable）
    /// 条件：readable 和 writable 指向不同目录
    /// 断言：readable-only 目录返回 Err，writable 目录返回 Ok
    #[test]
    fn check_dir_writable_uses_writable_roots() {
        let read_dir = TempDir::new().unwrap();
        let write_dir = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            read_dir.path(),
            Some(&[read_dir.path()]),  // readable
            Some(&[write_dir.path()]), // writable
        );

        // check_dir_writable on a readable-only dir should fail (it checks writable)
        let result = fs.check_dir_writable(read_dir.path());
        assert!(result.is_err());

        // check_dir_writable on a writable dir should succeed
        let result = fs.check_dir_writable(write_dir.path());
        assert!(result.is_ok());
    }

    // ══════════════════════════════════════════════════════════════
    //  Tests — unrestricted Fs (None roots)
    // ══════════════════════════════════════════════════════════════

    /// P0：无限制模式（Fs::new）可读取任意位置文件 [[Fs::read_to_string]]
    /// 条件：Fs::new("/tmp")，文件在其他临时目录
    /// 断言：读取成功返回 "hello"
    #[tokio::test]
    async fn unrestricted_fs_allows_read_anywhere() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("data.txt");
        stdfs::write(&file, "hello").unwrap();

        let fs = Fs::new("/tmp");
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello");
    }

    /// P0：[Fs::create_file] 无限制模式可创建文件
    /// 条件：Fs::new(dir)
    /// 断言：创建成功
    #[tokio::test]
    async fn unrestricted_fs_allows_create_file_anywhere() {
        let dir = TempDir::new().unwrap();
        let fs = Fs::new(dir.path());
        let result = fs.create_file("unrestricted.txt").await;
        assert!(result.is_ok());
    }

    /// P0：[Fs::atomic_write] 无限制模式可原子写入
    /// 条件：Fs::new(dir)
    /// 断言：写入成功，内容正确
    #[tokio::test]
    async fn unrestricted_fs_allows_atomic_write_anywhere() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("anywhere.txt");
        let fs = Fs::new(dir.path());
        let result = fs.atomic_write(&target, b"data", 0o600).await;
        assert!(result.is_ok());
        assert_eq!(stdfs::read_to_string(&target).unwrap(), "data");
    }

    /// P1：[Fs::read_to_string] readable=None 时不限制读操作
    /// 条件：new_with_permissions 的 readable 参数为 None
    /// 断言：读取成功返回 "hello"
    #[tokio::test]
    async fn none_readable_roots_allows_read() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("data.txt");
        stdfs::write(&file, "hello").unwrap();

        let fs = Fs::new_with_permissions(
            dir.path(),
            None,                // readable — unrestricted
            Some(&[dir.path()]), // writable
        );
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello");
    }

    /// P1：[Fs::create_file] writable=None 时不限制写操作
    /// 条件：new_with_permissions 的 writable 参数为 None
    /// 断言：创建成功
    #[tokio::test]
    async fn none_writable_roots_allows_write() {
        let dir = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(
            dir.path(),
            Some(&[dir.path()]), // readable
            None,                // writable — unrestricted
        );
        let result = fs.create_file("unrestricted-write.txt").await;
        assert!(result.is_ok());
    }

    // ══════════════════════════════════════════════════════════════
    //  补充测试 — 遗漏的分支路径
    // ══════════════════════════════════════════════════════════════

    /// P2：atomic_write 覆盖已有文件
    /// 条件：目标路径已存在一个旧文件
    /// 断言：写入成功，文件内容被更新为新值
    #[tokio::test]
    async fn atomic_write_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("overwrite.txt");
        stdfs::write(&target, "old").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.atomic_write(&target, b"new", 0o600).await;
        assert!(result.is_ok());
        assert_eq!(stdfs::read_to_string(&target).unwrap(), "new");
    }

    /// P2：read_to_string 读取非 UTF-8 文件返回 I/O 错误
    /// 条件：文件内容为无效 UTF-8 字节序列（0xFF 0xFE）
    /// 断言：返回 Err，错误信息包含 "Failed to read"
    #[tokio::test]
    async fn read_to_string_invalid_utf8_returns_io_err() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("binary.bin");
        stdfs::write(&file, [0xFF, 0xFE, 0x00, 0x01]).unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.read_to_string(&file).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Failed to read"),
            "non-UTF-8 content should produce 'Failed to read' error"
        );
    }

    /// P2：read_to_string 支持相对路径
    /// 条件：传入相对路径 "rel-read.txt"，文件在 cwd 下已存在
    /// 断言：读取成功，内容正确
    #[tokio::test]
    async fn read_to_string_relative_path() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("rel-read.txt"), "relative").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        assert_eq!(fs.read_to_string("rel-read.txt").await.unwrap(), "relative");
    }

    /// P1：check_readable 允许 roots 内的路径
    /// 条件：路径在 readable roots 内
    /// 断言：返回 Ok，返回的路径以原始文件名结尾
    #[test]
    fn check_readable_allows_path_within_roots() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("visible.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.check_readable(&file);
        assert!(result.is_ok());
        assert!(result.unwrap().ends_with("visible.txt"));
    }

    /// P1：check_readable 拒绝 roots 之外的路径
    /// 条件：路径在 forbidden 目录
    /// 断言：返回 Err
    #[test]
    fn check_readable_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.check_readable(forbidden.path().join("secret.txt"));
        assert!(result.is_err());
    }

    /// P1：check_writable 允许 roots 内的路径
    /// 条件：路径在 writable roots 内
    /// 断言：返回 Ok
    #[test]
    fn check_writable_allows_path_within_roots() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.check_writable(tmp.path().join("new-file.txt"));
        assert!(result.is_ok());
    }

    /// P1：check_writable 拒绝 roots 之外的路径
    /// 条件：路径在 forbidden 目录
    /// 断言：返回 Err
    #[test]
    fn check_writable_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.check_writable(forbidden.path().join("escape.txt"));
        assert!(result.is_err());
    }

    /// P0：[Fs::cwd] cwd() 返回构造时传入的工作目录
    /// 条件：使用 Fs::new 构造
    /// 断言：cwd() 返回值等于构造时传入的路径
    #[test]
    fn cwd_returns_working_directory() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new(tmp.path());
        assert_eq!(fs.cwd(), tmp.path());
    }

    /// P1：[Fs::create_file_unique] create_file_unique 支持相对路径
    /// 条件：传入相对路径 "rel-unique.txt"
    /// 断言：创建成功，文件名为 "rel-unique.txt"
    #[tokio::test]
    async fn create_file_unique_relative_path() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let (path, _) = fs.create_file_unique("rel-unique.txt").await.unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "rel-unique.txt");
    }

    /// P1：[Fs::metadata] metadata 支持相对路径
    /// 条件：传入相对路径 "rel-meta.txt"，文件已存在
    /// 断言：获取成功，文件大小正确
    #[tokio::test]
    async fn metadata_relative_path() {
        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join("rel-meta.txt"), "abc").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let meta = fs.metadata("rel-meta.txt").await.unwrap();
        assert_eq!(meta.len(), 3);
    }

    /// P1：[Fs::remove_file] 无限制模式下允许删除文件
    /// 条件：Fs::new 创建无限制实例
    /// 断言：删除成功，文件不再存在
    #[tokio::test]
    async fn unrestricted_fs_allows_remove_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("to-delete.txt");
        stdfs::write(&file, "bye").unwrap();

        let fs = Fs::new(dir.path());
        fs.remove_file(&file).await.unwrap();
        assert!(!file.exists());
    }

    /// P1：[Fs::remove_dir_all] 无限制模式下允许递归删除目录
    /// 条件：Fs::new 创建无限制实例
    /// 断言：删除成功，目录不再存在
    #[tokio::test]
    async fn unrestricted_fs_allows_remove_dir_all() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        stdfs::create_dir_all(sub.join("nested")).unwrap();
        stdfs::write(sub.join("nested/f.txt"), "data").unwrap();

        let fs = Fs::new(dir.path());
        fs.remove_dir_all(&sub).await.unwrap();
        assert!(!sub.exists());
    }

    /// P1：[Fs::list_dir] 无限制模式下允许列出目录
    /// 条件：Fs::new 创建无限制实例
    /// 断言：返回正确的文件列表
    #[tokio::test]
    async fn unrestricted_fs_allows_list_dir() {
        let dir = TempDir::new().unwrap();
        stdfs::write(dir.path().join("a.txt"), "a").unwrap();
        stdfs::write(dir.path().join("b.txt"), "b").unwrap();

        let fs = Fs::new(dir.path());
        let files: Vec<_> = fs
            .list_dir(dir.path())
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
        assert_eq!(files.len(), 2);
    }

    /// P1：[Fs::metadata] 无限制模式下允许获取元数据
    /// 条件：Fs::new 创建无限制实例
    /// 断言：获取成功，文件大小正确
    #[tokio::test]
    async fn unrestricted_fs_allows_metadata() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("info.txt");
        stdfs::write(&file, "hello").unwrap();

        let fs = Fs::new(dir.path());
        let meta = fs.metadata(&file).await.unwrap();
        assert_eq!(meta.len(), 5);
    }

    // ── Debug / set_cwd / _mut accessors ──

    /// P2：[Fs::Debug] 格式化包含 cwd、readable_dirs、writable_dirs 字段
    /// 条件：构造受限 Fs，调用 format!("{:?}", fs)
    /// 断言：输出包含 cwd、readable_dirs、writable_dirs
    #[test]
    fn debug_fmt_includes_field_names() {
        let dir = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(dir.path(), Some(&[dir.path()]), Some(&[dir.path()]));
        let debug = format!("{fs:?}");
        assert!(debug.contains("cwd"));
        assert!(debug.contains("readable_dirs"));
        assert!(debug.contains("writable_dirs"));
    }

    /// P2：[Fs::set_cwd] 修改当前工作目录
    /// 条件：先 Fs::new("/old")，再 set_cwd("/new")
    /// 断言：cwd() 返回 "/new"
    #[test]
    fn set_cwd_updates_cwd() {
        let mut fs = Fs::new("/old");
        assert_eq!(fs.cwd(), Path::new("/old"));
        fs.set_cwd("/new");
        assert_eq!(fs.cwd(), Path::new("/new"));
    }

    /// P2：[Fs::readable_dirs_mut] 返回可变引用可修改
    /// 条件：构造 Fs 含 readable_dirs = Some([p])，通过 _mut 设为 None
    /// 断言：readable_dirs() 返回 None
    #[test]
    fn readable_dirs_mut_allows_setting_none() {
        let dir = TempDir::new().unwrap();
        let mut fs = Fs::new_with_permissions(dir.path(), Some(&[dir.path()]), Some(&[dir.path()]));
        let dirs_mut = fs.readable_dirs_mut();
        *dirs_mut = None;
        assert!(fs.readable_dirs().is_none());
    }

    /// P2：[Fs::writable_dirs_mut] 返回可变引用可修改
    /// 条件：构造 Fs 含 writable_dirs = Some([p])，通过 _mut 设为 None
    /// 断言：writable_dirs() 返回 None
    #[test]
    fn writable_dirs_mut_allows_setting_none() {
        let dir = TempDir::new().unwrap();
        let mut fs = Fs::new_with_permissions(dir.path(), Some(&[dir.path()]), Some(&[dir.path()]));
        let dirs_mut = fs.writable_dirs_mut();
        *dirs_mut = None;
        assert!(fs.writable_dirs().is_none());
    }

    // ── open_for_read ──

    /// P1：[Fs::open_for_read] 成功打开沙盒内文件
    /// 条件：文件在 readable roots 内，调用 open_for_read()
    /// 断言：返回 Ok，含 resolved path 和可读取的 File
    #[tokio::test]
    async fn open_for_read_success() {
        use tokio::io::AsyncReadExt;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("stream.txt");
        stdfs::write(&file, "streaming content").unwrap();

        let fs = Fs::new_with_permissions(dir.path(), Some(&[dir.path()]), Some(&[dir.path()]));
        let (resolved, mut tokio_file) = fs.open_for_read(&file).await.unwrap();
        assert!(resolved.ends_with("stream.txt"));

        let mut buf = String::new();
        tokio_file.read_to_string(&mut buf).await.unwrap();
        assert_eq!(buf, "streaming content");
    }

    /// P1：[Fs::open_for_read] 拒绝打开 roots 外文件
    /// 条件：文件在 readable roots 外
    /// 断言：返回 Err
    #[tokio::test]
    async fn open_for_read_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let file = forbidden.path().join("secret.txt");
        stdfs::write(&file, "secret").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        assert!(fs.open_for_read(&file).await.is_err());
    }

    // ── resolve_readable_or_suggest ──

    /// P0：[resolve_readable_or_suggest] 直接解析成功，不触发模糊纠错（限制模式）
    /// 条件：文件在 readable roots 内且正常可读
    /// 断言：返回 Ok，结果路径以原文件名结尾
    #[tokio::test]
    async fn resolve_readable_direct_success_restricted() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("good.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs
            .resolve_readable_or_suggest(&file.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("good.txt"));
    }

    /// P0：[resolve_readable_or_suggest] 直接解析成功，不触发模糊纠错（无限制模式）
    /// 条件：文件存在，Fs 无限制
    /// 断言：返回 Ok，路径正确
    #[tokio::test]
    async fn resolve_readable_direct_success_unrestricted() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        stdfs::write(&file, "world").unwrap();

        let fs = Fs::new(tmp.path());
        let result = fs
            .resolve_readable_or_suggest(&file.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("hello.txt"));
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：trie 锚定失败保留原始逃逸错误
    /// 条件：文件不在任何 readable root 下，且无接近 root 可锚定
    /// 断言：返回 Err，错误信息包含 "目标路径超出可访问范围"（保留原始错误而非 "找不到目标文件"）
    #[tokio::test]
    async fn resolve_readable_restricted_trie_fails() {
        let allowed = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("orphan.txt");
        stdfs::write(&file, "data").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs
            .resolve_readable_or_suggest(&file.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("目标路径超出可访问范围"),
            "should preserve original escape error, msg = {msg}"
        );
    }

    /// P0：[resolve_readable_or_suggest] 限制模式：trie 模糊锚定后逐级精确匹配成功
    /// 条件：可读根 /workspace，根下有子目录 sub 含文件 file.txt；
    ///       输入 /wrkspace/sub/file.txt（root 名有 1 字符笔误），check_readable 失败
    /// 断言：模糊纠错成功，返回正确路径以 file.txt 结尾
    #[tokio::test]
    async fn resolve_readable_fuzzy_typo_then_exact_walk() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(workspace.join("sub")).unwrap();
        stdfs::write(workspace.join("sub/file.txt"), "data").unwrap();

        // Readable root = <tmp>/workspace, so <tmp>/wrkspace/... won't pass check_readable
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Path with typo: "wrkspace" instead of "workspace" — 1 char deletion.
        let path = tmp.path().join("wrkspace/sub/file.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("file.txt"));
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：模糊锚定后 per-level 空目录报错
    /// 条件：可读根 /workspace，根下有 empty 空目录；
    ///       输入 /wrkspace/empty/missing.txt（root 笔误 + 空目录无候选）
    /// 断言：trie 锚定成功，但 empty 下无候选 → Err
    #[tokio::test]
    async fn resolve_readable_fuzzy_typo_then_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(workspace.join("empty")).unwrap();
        stdfs::write(workspace.join("marker.txt"), "x").unwrap();

        // Readable root = <tmp>/workspace, typo path = <tmp>/wrkspace/...
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        let path = tmp.path().join("wrkspace/empty/missing.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：trie 锚定成功但 tail 超长被 trie 拒绝
    /// 条件：可读根 /root，路径含 17 段 tail（> WALK_DEPTH_MAX），root 名有 1 笔误
    /// 断言：返回 Err，错误信息包含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_trie_tail_too_long() {
        let tmp = TempDir::new().unwrap();
        let root_dir = tmp.path().join("root");
        stdfs::create_dir_all(&root_dir).unwrap();

        // Readable root = <tmp>/root, typo path = <tmp>/rrot/...
        let fs =
            Fs::new_with_permissions(&root_dir, Some(&[root_dir.as_path()]), Some(&[tmp.path()]));
        // 17 tail segments → trie rejects ( > WALK_DEPTH_MAX )
        let mut deep = tmp.path().join("rrot");
        for i in 0..17 {
            deep = deep.join(format!("level{}", i));
        }
        let result = fs
            .resolve_readable_or_suggest(&deep.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：per-level 模糊匹配置信度不足
    /// 条件：可读根 /workspace，根下有 file_a.txt 和 file_b.txt 两个文件；
    ///       输入 /wrkspace/file_x.txt（root 笔误 + 文件名无接近候选）
    /// 断言：trie 锚定成功，但文件名段 pick abort → Err（含 "找不到目标文件"）
    #[tokio::test]
    async fn resolve_readable_per_level_pick_abort() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("file_a.txt"), "a").unwrap();
        stdfs::write(workspace.join("file_b.txt"), "b").unwrap();

        // Readable root = <tmp>/workspace, typo path = <tmp>/wrkspace/...
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // "file_x.txt" vs ["file_a.txt", "file_b.txt"]
        // distances: x→a=1, x→b=1 → tie → Abort
        let path = tmp.path().join("wrkspace/file_x.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P1：[resolve_readable_or_suggest] 无限制模式：模糊纠错成功（最深存在祖先 + 逐级下潜）
    /// 条件：文件在 /a/docspace/f.md，输入 /a/docs pace/f.md（父目录段含空格）
    /// 断言：模糊纠错成功，返回 /a/docspace/f.md
    #[tokio::test]
    async fn resolve_readable_unrestricted_fuzzy_corrects() {
        let tmp = TempDir::new().unwrap();
        let docspace = tmp.path().join("docspace");
        stdfs::create_dir_all(&docspace).unwrap();
        stdfs::write(docspace.join("f.md"), "data").unwrap();

        // Unrestricted Fs with cwd = tmp, path has space typo
        let fs = Fs::new(tmp.path());
        let path = tmp.path().join("docs pace/f.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("f.md"));
        assert!(result.to_string_lossy().contains("docspace"));
    }

    /// P1：[resolve_readable_or_suggest] 无限制模式：无接近条目放弃纠错
    /// 条件：目录 /a 下无任何接近 "zzzzz" 的文件
    /// 断言：Abort → 返回原错误
    #[tokio::test]
    async fn resolve_readable_unrestricted_no_close_entry() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        stdfs::create_dir_all(&a).unwrap();
        stdfs::write(a.join("some_other_file.md"), "data").unwrap();

        let fs = Fs::new(tmp.path());
        let path = tmp.path().join("a/zzzzz_data.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：root 内但文件不存在且无相近候选 → 报错
    /// 条件：可读根 /root（空目录），路径 /root/nonexistent.md 文件不存在
    /// 断言：返回 Err（两种模式都要求文件存在；空目录无候选）含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_restricted_nonexistent_no_candidate_errs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        stdfs::create_dir_all(&root).unwrap();

        let fs = Fs::new_with_permissions(&root, Some(&[root.as_path()]), Some(&[root.as_path()]));
        // File does NOT exist and the dir has no close candidate.
        let path = root.join("nonexistent.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P0：[resolve_readable_or_suggest] 限制模式：root 段正确、仅文件名段噪声也能纠正
    /// 条件：可读根 /root，根下有 "data (1).md"；输入 /root/data(1).md（在 root 内但文件不存在）
    /// 断言：Ok，返回以 "data (1).md" 结尾（要求 exists() 后，root 内文件名噪声也进入纠错）
    #[tokio::test]
    async fn resolve_readable_restricted_within_root_filename_noise_corrected() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        stdfs::write(root.join("data (1).md"), "one").unwrap();

        let fs = Fs::new_with_permissions(&root, Some(&[root.as_path()]), Some(&[root.as_path()]));
        // Root segment is correct; only the filename lacks a space. The path is
        // within roots but does not exist → must still be corrected.
        let path = root.join("data(1).md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("data (1).md"), "got: {}", result.display());
    }

    /// P1：[resolve_readable_or_suggest] 无限制模式：tail 超过 WALK_DEPTH_MAX 报错
    /// 条件：路径的父级以下有超过 16 层的子目录/文件，即尾段数 > 16
    /// 断言：返回 Err，错误信息包含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_unrestricted_tail_too_long() {
        let tmp = TempDir::new().unwrap();
        // Only tmp root exists; deep path with 17+ tail segments.
        let mut deep = tmp.path().to_path_buf();
        // 18 segments beyond tmp → tail = 18 > WALK_DEPTH_MAX = 16
        for i in 0..18 {
            deep = deep.join(format!("level{i}"));
        }

        let fs = Fs::new(tmp.path());
        let result = fs
            .resolve_readable_or_suggest(&deep.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    // ── resolve_readable_or_suggest additional coverage ──

    /// P1：[resolve_readable_or_suggest] 限制模式：阶段二中间目录段 pick abort（平局）
    /// 条件：可读根 /workspace，根下有 dir_a/file.txt 和 dir_b/file.txt；
    ///       输入 /wrkspace/dir_x/file.txt（root 笔误纠正后，dir_x 对 dir_a/dir_b 平局）
    /// 断言：Err，错误信息含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_per_level_abort_at_intermediate_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(workspace.join("dir_a")).unwrap();
        stdfs::write(workspace.join("dir_a/file.txt"), "a").unwrap();
        stdfs::create_dir_all(workspace.join("dir_b")).unwrap();
        stdfs::write(workspace.join("dir_b/file.txt"), "b").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Input: /wrkspace/dir_x/file.txt
        // Phase 1: trie anchors to /workspace via fuzzy root correction, tail=[dir_x, file.txt]
        // Phase 2: cur=/workspace, list_dir → [dir_a, dir_b]
        //   pick("dir_x", ["dir_a", "dir_b"]): dd1=1, dd2=1 → tie → Abort
        let path = tmp.path().join("wrkspace/dir_x/file.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：阶段二中间目录段被纠正
    /// 条件：可读根 /workspace，根下有 sub/images/pic.png；
    ///       输入 /wrkspace/sb/images/pic.png（root 笔误 + 中间目录 "sb" 纠正为 "sub"）
    /// 断言：Ok，返回路径以 pic.png 结尾且路径包含 sub（非 sb）
    #[tokio::test]
    async fn resolve_readable_correction_at_intermediate_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(workspace.join("sub").join("images")).unwrap();
        stdfs::write(workspace.join("sub/images/pic.png"), "image data").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Input: /wrkspace/sb/images/pic.png
        // Phase 1: trie anchors to /workspace, tail=[sb, images, pic.png]
        // Phase 2: cur=/workspace, list_dir → [sub], pick("sb", ["sub"]): dd1=1 → Corrected("sub")
        //          cur=/workspace/sub, list_dir → [images], pick("images",["images"]): dd1=0 → Exact
        //          cur=/workspace/sub/images, list_dir → [pic.png], pick("pic.png",["pic.png"]): dd1=0 → Exact
        let path = tmp.path().join("wrkspace/sb/images/pic.png");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("pic.png"));
        assert!(result.to_string_lossy().contains("sub"));
    }

    /// P0：[resolve_readable_or_suggest] 限制模式：逃逸 + 文件名空格错位经空格不敏感档纠正
    /// 条件：可读根 /workspace，根下有 "data (1).md" 与 "data (2).md"；
    ///       输入 /wrkspace/data(1).md（root 笔误逃逸 + 文件名少 1 空格）
    /// 断言：Ok，返回路径以 "data (1).md" 结尾（去空格唯一命中 (1)，不误纠成 (2)）
    #[tokio::test]
    async fn resolve_readable_filename_whitespace_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("data (1).md"), "one").unwrap();
        stdfs::write(workspace.join("data (2).md"), "two").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Root typo "wrkspace" → escape → phase 1 anchors /workspace;
        // filename "data(1).md" → whitespace-insensitive unique match → "data (1).md".
        let path = tmp.path().join("wrkspace/data(1).md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("data (1).md"), "got: {}", result.display());
    }

    // NOTE: LEVEL_FANOUT_MAX guard cannot be efficiently unit-tested
    // (requires 10000+ directory entries).  The `entries.is_empty()` branch
    // inside the same `||` is covered by `resolve_readable_fuzzy_typo_then_empty_dir`.

    /// P0：[resolve_readable_or_suggest] 限制模式：多 sibling root + root 名空格噪声
    /// 条件：可读根 /skills、/data、/workspace，输入 /work space/data1.md（root 名插入 1 空格）
    /// 断言：Ok，返回路径包含 workspace 且以 data1.md 结尾（trie 首层 pick 消化空格）
    #[tokio::test]
    async fn resolve_readable_multi_sibling_root_name_noise() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let data = tmp.path().join("data");
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&skills).unwrap();
        stdfs::create_dir_all(&data).unwrap();
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("data1.md"), "content").unwrap();

        let fs = Fs::new_with_permissions(
            tmp.path(),
            Some(&[skills.as_path(), data.as_path(), workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Root name "work space" has an extra space → escape → trie anchors /workspace
        let path = tmp.path().join("work space/data1.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("data1.md"), "got: {}", result.display());
        assert!(
            result.to_string_lossy().contains("workspace"),
            "got: {}",
            result.display()
        );
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：多 sibling root 锚定失败保留原始错误
    /// 条件：可读根 /skills、/data、/workspace，输入 /zzzzz/data1.md（首段无接近 root）
    /// 断言：返回 Err，错误信息含 "目标路径超出可访问范围"（保留原始逃逸错误而非 "找不到目标文件"）
    #[tokio::test]
    async fn resolve_readable_multi_sibling_anchor_fail_preserves_error() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let data = tmp.path().join("data");
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&skills).unwrap();
        stdfs::create_dir_all(&data).unwrap();
        stdfs::create_dir_all(&workspace).unwrap();

        let fs = Fs::new_with_permissions(
            tmp.path(),
            Some(&[skills.as_path(), data.as_path(), workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // "zzzzz" is not close to any root name → trie anchor fails
        let path = tmp.path().join("zzzzz/data1.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("目标路径超出可访问范围"),
            "should preserve original escape error, got: {msg}"
        );
    }

    // ── resolve_readable_or_suggest: boundary / edge cases ──

    /// P1：[resolve_readable_or_suggest] 限制模式：tail 段数恰为 WALK_DEPTH_MAX (16) 允许通过
    /// 条件：可读根 /workspace，根下有 15 层嵌套子目录 + 1 个文件（tail.len() == 16）
    /// 断言：Ok，返回路径以最终文件名结尾
    #[tokio::test]
    async fn resolve_readable_tail_at_walk_depth_max_boundary() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        // Build 15 nested dirs under workspace, then a file at the deepest level.
        let mut cur = workspace.clone();
        for i in 0..15 {
            cur = cur.join(format!("d{i}"));
        }
        stdfs::create_dir_all(&cur).unwrap();
        stdfs::write(cur.join("final.txt"), "deep").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Input: /tmp/.../wrkspace/d0/d1/.../d14/final.txt
        // Root typo "wrkspace" → trie anchors /workspace, tail = 16 segments (= WALK_DEPTH_MAX)
        let mut path = tmp.path().join("wrkspace");
        for i in 0..15 {
            path = path.join(format!("d{i}"));
        }
        path = path.join("final.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("final.txt"), "got: {}", result.display());
    }

    /// P1：[resolve_readable_or_suggest] 直接解析：输入路径是已存在目录（非文件）也返回 Ok
    /// 条件：工作目录在可读根内，且作为目录存在
    /// 断言：Ok（check_readable + exists() 对目录为 true，直接返回，不触发纠错）
    #[tokio::test]
    async fn resolve_readable_direct_ok_for_directory() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        // Input is the readable root itself (a directory, not a file).
        let result = fs
            .resolve_readable_or_suggest(&workspace.to_string_lossy())
            .await
            .unwrap();
        // Returns the canonicalised path of the directory.
        assert!(result.ends_with("workspace"), "got: {}", result.display());
    }

    /// P1：[resolve_readable_or_suggest] 限制模式：输入为 root 名笔误且无 tail → Err
    /// 条件：可读根 /workspace，输入 /wrkspace（root 名笔误，无文件段）；trie 锚定后 tail 为空
    /// 断言：返回 Err，错误信息含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_input_is_typo_root_empty_tail() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Path is just the typo root name — trie anchors to /workspace but tail is empty.
        let path = tmp.path().join("wrkspace");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P1：[resolve_readable_or_suggest] 阶段二：tail 中间段是文件（非目录）导致过滤后无候选
    /// 条件：可读根 /workspace，根下有文件 existing.txt；输入 /workspace/existing.txt/extra.md
    ///       check_readable 通过但文件不存在 → 进入阶段二；walk_tail 中间段 is_dir() 过滤掉文件
    /// 断言：Err，错误信息含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_intermediate_segment_is_file() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("existing.txt"), "data").unwrap();

        // Readable root = workspace, so the path is within roots.
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        // "existing.txt" is a file, not a dir → is_dir() filters it out.
        let path = workspace.join("existing.txt").join("extra.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P2：[resolve_readable_or_suggest] CJK/Unicode 文件名单字符笔误被纠正
    /// 条件：文件名为 "报告模板.md"，输入 "报告模版.md"（"板"→"版" 一字替换，ed=1）
    /// 断言：Ok，返回路径以 "报告模板.md" 结尾
    #[tokio::test]
    async fn resolve_readable_cjk_filename_typo_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("报告模板.md"), "季度报告内容").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // "报告模版.md" vs "报告模板.md": 1 char substitution — within tolerance.
        let path = tmp.path().join("wrkspace/报告模版.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("报告模板.md"), "got: {}", result.display());
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：tail 两段均有噪声且均被纠正
    /// 条件：可读根 /workspace，根下有 sub_a/file_s.md（目标）和
    ///       sub_longname_dir + file_distractor_very_long.md（干扰项，ed 差距大保证裕度）
    ///       输入 /wrkspace/sub_b/file_x.md（root 笔误 + dir 1替换 + file 1替换）
    /// 断言：Ok，每段均通过相关性+裕度闸门，返回正确路径
    #[tokio::test]
    async fn resolve_readable_two_tail_segments_both_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        // Two sibling dirs: sub_a (target) and sub_longname_dir (distractor with large ed).
        stdfs::create_dir_all(workspace.join("sub_a")).unwrap();
        stdfs::create_dir_all(workspace.join("sub_longname_dir")).unwrap();
        // Two files in sub_a: file_s.md (target) and distractor with large ed.
        stdfs::write(workspace.join("sub_a/file_s.md"), "content").unwrap();
        stdfs::write(
            workspace.join("sub_a/file_distractor_very_long.md"),
            "distractor",
        )
        .unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Input: /wrkspace/sub_b/file_x.md
        // Phase 1: trie anchors /workspace, tail = [sub_b, file_x.md]
        // Phase 2 step 1: pick("sub_b", ["sub_a","sub_longname_dir"])
        //   dd1(sub_b,sub_a)=1, dd2(sub_b,sub_longname_dir)=10
        //   Gate: dd1=1 ≤ r_cap(clamp(floor(5*0.34),1,3)=1) ✓
        //   dd2-dd1=9 ≥ 2 ✓, 10*2=20 ≥ 3 ✓ → Corrected("sub_a")
        // Step 2: pick("file_x.md", ["file_s.md","file_distractor_very_long.md"])
        //   dd1(file_x.md,file_s.md)=1, dd2≈26
        //   Gate: dd1=1 ≤ r_cap ✓, dd2-dd1≈25 ≥ 2 ✓ → Corrected("file_s.md")
        let path = tmp.path().join("wrkspace/sub_b/file_x.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("file_s.md"), "got: {}", result.display());
        assert!(
            result.to_string_lossy().contains("sub_a"),
            "got: {}",
            result.display()
        );
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：dotfile（隐藏文件）文件名噪声被纠正
    /// 条件：可读根 /workspace，根下有 ".gitignore"；输入 ".gitignor"（缺尾字母 e, ed=1）
    /// 断言：Ok，返回路径以 ".gitignore" 结尾
    #[tokio::test]
    async fn resolve_readable_dotfile_typo_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join(".gitignore"), "# build\n").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // ".gitignor" vs ".gitignore": 1 char deletion, within tolerance.
        let path = tmp.path().join("wrkspace/.gitignor");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with(".gitignore"), "got: {}", result.display());
    }

    /// P1：[resolve_readable_or_suggest] 无限制模式：阶段二中间段 pick abort → Err
    /// 条件：目录 /a/docspace 下有 dir_x/file.txt 和 dir_y/file.txt；
    ///       输入 /a/docspace/dir_z/file.txt——"dir_z" 对 "dir_x"/"dir_y" 平局
    /// 断言：Err，错误信息含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_unrestricted_per_level_abort() {
        let tmp = TempDir::new().unwrap();
        let docspace = tmp.path().join("docspace");
        stdfs::create_dir_all(docspace.join("dir_x")).unwrap();
        stdfs::write(docspace.join("dir_x/file.txt"), "x").unwrap();
        stdfs::create_dir_all(docspace.join("dir_y")).unwrap();
        stdfs::write(docspace.join("dir_y/file.txt"), "y").unwrap();

        let fs = Fs::new(tmp.path());
        // Unrestricted: deepest_existing_ancestor anchors at /tmp/docspace,
        // tail = [dir_z, file.txt]
        // pick("dir_z", ["dir_x","dir_y"]): dd1=1, dd2=1 → tie → Abort
        let path = tmp.path().join("docspace/dir_z/file.txt");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：tail 仅文件段，噪声是空格插入且唯一候选
    /// 条件：可读根 /workspace，根下有 "hello world.md"；输入 /workspace/helloworld.md
    /// 断言：Ok，空格不敏感唯一匹配返回 "hello world.md"
    #[tokio::test]
    async fn resolve_readable_filename_space_insertion_unique() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("hello world.md"), "content").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        // Within roots, file does not exist as "helloworld.md" → correction.
        // "helloworld.md" → noise-insensitive match to "hello world.md" (unique).
        let path = workspace.join("helloworld.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(
            result.ends_with("hello world.md"),
            "got: {}",
            result.display()
        );
    }

    /// P2：[resolve_readable_or_suggest] 限制模式：root + 中间段 + 文件段三级全噪声且全部纠正
    /// 条件：可读根 /workspace，根下有 projects/web_app/index.html
    ///       输入 /workspc/proects/webapp/index.htm（root 删 1 字符、dir 删 ject、file 少 l）
    /// 断言：Ok，每段均被逐级纠正
    #[tokio::test]
    async fn resolve_readable_three_level_chain_correction() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(workspace.join("projects").join("web_app")).unwrap();
        stdfs::write(
            workspace.join("projects/web_app/index.html"),
            "<html></html>",
        )
        .unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // All three non-root segments carry noise:
        //   root: "workspc" → "workspace" (1 deletion)
        //   dir:  "proects" → "projects" (1 deletion: 'j')
        //   file: "index.htm" → "index.html" (1 deletion: 'l')
        // Each level has a clear winning candidate: tie-breakers pass.
        let path = tmp.path().join("workspc/proects/web_app/index.htm");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.ends_with("index.html"), "got: {}", result.display());
        assert!(
            result.to_string_lossy().contains("projects"),
            "got: {}",
            result.display()
        );
    }

    // ── platform-specific considerations ──
    //
    // The resolution pipeline relies on `Path::components()` which is
    // platform-aware.  On Windows / DOS, paths carry `Prefix` components
    // (e.g. `C:`) that `RootTrie` treats as structural (exact-match only).
    // The `pick` primitive uses `strsim::levenshtein` which is case-sensitive
    // — on case-insensitive filesystems (macOS APFS default, Windows NTFS),
    // the noise-insensitive tier (whitespace + case folding) compensates.
    //
    // Platform-specific behaviour that may differ:
    //  - Case preservation: file created as "File.TXT" on macOS resolves as
    //    "file.txt" after `canonicalize()`; the noise-insensitive tier
    //    handles this.
    //  - Windows reserved names (CON, NUL, PRN) are avoided by `sanitize_filename`.
    //  - Path length limits (260 chars on Windows) may cause `canonicalize()`
    //    to fail for deep structures like `resolve_readable_tail_at_walk_depth_max_boundary`.
    //  - Unicode normalisation (macOS HFS+ NFD vs Linux NFC) may affect
    //    precomposed vs decomposed character matching — `levenshtein` treats
    //    them as different characters.

    /// P1：[resolve_readable_or_suggest] 限制模式：第二阶段文件段 pick abort 无歧义
    /// （补充：测试 dd1 通过相关性但 dd2 与 dd1 平局的情况，覆盖未测的裕度闸门路径）
    /// 条件：可读根 /workspace，根下有 cat.md 和 car.md；
    ///       输入 /wrkspace/caw.md（root 笔误 + 文件名对两文件距离均为 1）
    /// 断言：Err，含 "找不到目标文件"
    #[tokio::test]
    async fn resolve_readable_file_level_tie_no_correction() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        stdfs::write(workspace.join("cat.md"), "meow").unwrap();
        stdfs::write(workspace.join("car.md"), "vroom").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // "caw.md" distance to "cat.md" = 1, to "car.md" = 1 → tie → Abort
        let path = tmp.path().join("wrkspace/caw.md");
        let result = fs
            .resolve_readable_or_suggest(&path.to_string_lossy())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("找不到目标文件"), "msg = {msg}");
    }

    // ── resolve_writable_or_suggest ──

    /// P0：[resolve_writable_or_suggest] 限制模式：直接可写，快速路径返回
    /// 条件：writable roots 包含 tmp，file_path 指向 tmp 下路径
    /// 断言：返回 Ok，路径为 resolve_real_path 结果
    #[tokio::test]
    async fn resolve_writable_direct_success_restricted() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("workspace");
        stdfs::create_dir_all(&ws).unwrap();
        let fs = Fs::new_with_permissions(&ws, Some(&[ws.as_path()]), Some(&[ws.as_path()]));
        let path = ws.join("out.json");
        let result = fs
            .resolve_writable_or_suggest(&path)
            .await
            .expect("direct writable should succeed");
        assert!(result.ends_with("out.json"));
    }

    /// P0：[resolve_writable_or_suggest] 限制模式：root 笔误被纠正
    /// 条件：writable root = /workspace，输入 /wrkspace/out.json
    /// 断言：返回 Ok，路径以 /workspace/out.json 结尾
    #[tokio::test]
    async fn resolve_writable_root_typo_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        // "wrkspace" vs "workspace": 1 char deletion
        let path = tmp.path().join("wrkspace/out.json");
        let result = fs
            .resolve_writable_or_suggest(&path)
            .await
            .expect("root typo should be corrected");
        assert!(result.ends_with("out.json"));
        assert!(result.to_string_lossy().contains("workspace"));
    }

    /// P1：[resolve_writable_or_suggest] 限制模式：超出沙箱 → Error::Permission
    /// 条件：writable root = /workspace，文件在允许范围外的 /forbidden/
    /// 断言：Err，含 Permission
    #[tokio::test]
    async fn resolve_writable_outside_roots_permission_denied() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let allowed_dir = allowed.path().to_path_buf();
        let fs = Fs::new_with_permissions(
            &allowed_dir,
            Some(&[allowed_dir.as_path()]),
            Some(&[allowed_dir.as_path()]),
        );
        let path = forbidden.path().join("out.json");
        let result = fs.resolve_writable_or_suggest(&path).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("目标路径超出可访问范围"), "msg = {msg}");
    }

    /// P0：[resolve_writable_or_suggest] 无限制模式：正常写入
    /// 条件：writable_dirs = None，路径任意
    /// 断言：Ok
    #[tokio::test]
    async fn resolve_writable_unrestricted_ok() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new(tmp.path());
        let path = tmp.path().join("new_file.txt");
        let result = fs
            .resolve_writable_or_suggest(&path)
            .await
            .expect("unrestricted writable should succeed");
        assert!(result.ends_with("new_file.txt"));
    }

    /// P1：[resolve_writable_or_suggest] 无限制模式：不存在的路径直接解析通过
    /// 条件：writable_dirs = None，路径不存在
    /// 断言：Ok（无限制模式下不校验路径是否存在）
    #[tokio::test]
    async fn resolve_writable_unrestricted_nonexistent_path_ok() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new(tmp.path());
        let path = PathBuf::from("/nonexistent_xyz/sub/file.txt");
        let result = fs
            .resolve_writable_or_suggest(&path)
            .await
            .expect("unrestricted mode accepts any path");
        // 路径直接解析通过
        assert!(result.to_string_lossy().contains("nonexistent_xyz"));
    }

    /// P0：[resolve_writable_or_suggest] 限制模式：tail 段直接拼接不做纠错
    /// 条件：writable root = /workspace，输入 /wrkspace/new_dir/maybe_new.txt
    /// 断言：Ok，tail "new_dir/maybe_new.txt" 直接拼接，不逐级匹配
    #[tokio::test]
    async fn resolve_writable_tail_joined_verbatim_no_fuzzy_walk() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        // 中间段和最终文件都不存在，应直接拼接
        let path = tmp.path().join("wrkspace/new_dir/maybe_new.txt");
        let result = fs
            .resolve_writable_or_suggest(&path)
            .await
            .expect("should anchor root and join tail verbatim");
        assert!(result.ends_with("maybe_new.txt"));
        assert!(result.to_string_lossy().contains("new_dir"));
        assert!(result.to_string_lossy().contains("workspace"));
    }

    // ── resolve_dir_writable_or_suggest ──

    /// P0：[resolve_dir_writable_or_suggest] 正常目录
    /// 条件：writable roots 包含 tmp
    /// 断言：Ok
    #[tokio::test]
    async fn resolve_dir_writable_direct_success() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("outputs");
        stdfs::create_dir_all(&dir).unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs
            .resolve_dir_writable_or_suggest(&dir)
            .await
            .expect("existing dir should succeed");
        assert!(result.ends_with("outputs"));
    }

    /// P0：[resolve_dir_writable_or_suggest] 不存在的目录也允许
    /// 条件：目标目录尚未创建
    /// 断言：Ok
    #[tokio::test]
    async fn resolve_dir_writable_nonexistent_ok() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("future_outputs");
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs
            .resolve_dir_writable_or_suggest(&dir)
            .await
            .expect("nonexistent dir should be allowed");
        assert!(result.ends_with("future_outputs"));
    }

    /// P1：[resolve_dir_writable_or_suggest] 目标为已存在文件 → Error::Validation
    /// 条件：目标是一个普通文件
    /// 断言：Err，含 "无效目录路径"
    #[tokio::test]
    async fn resolve_dir_writable_existing_file_err() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("some_file.txt");
        stdfs::write(&file, b"data").unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs.resolve_dir_writable_or_suggest(&file).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("无效目录路径"), "msg = {msg}");
    }

    /// P1：[resolve_dir_writable_or_suggest] root 笔误被纠正（目录版）
    /// 条件：writable root = /workspace，输入 /wrkspace/outputs
    /// 断言：Ok
    #[tokio::test]
    async fn resolve_dir_writable_root_typo_corrected() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        stdfs::create_dir_all(&workspace).unwrap();
        let outputs = workspace.join("outputs");
        stdfs::create_dir_all(&outputs).unwrap();
        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[workspace.as_path()]),
        );
        let path = tmp.path().join("wrkspace/outputs");
        let result = fs
            .resolve_dir_writable_or_suggest(&path)
            .await
            .expect("dir root typo should be corrected");
        assert!(result.ends_with("outputs"));
        assert!(result.to_string_lossy().contains("workspace"));
    }

    // ── check_file_size_limit ──

    /// P0：[check_file_size_limit] 文件小于限制时返回 Ok
    /// 条件：创建 10 字节文件，max_size = 1024
    /// 断言：返回 Ok(())
    #[tokio::test]
    async fn check_file_size_limit_small_file_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("small.txt");
        std::fs::write(&file_path, b"1234567890").unwrap();
        let file_path_str = file_path.to_string_lossy().to_string();

        let fs = Fs::new(tmp.path());
        let result = check_file_size_limit(&fs, &file_path_str, 1024).await;
        assert!(result.is_ok());
    }

    /// P0：[check_file_size_limit] 文件超过限制时返回 Err
    /// 条件：创建 10 字节文件，max_size = 4
    /// 断言：返回 Err，错误信息包含文件路径
    #[tokio::test]
    async fn check_file_size_limit_oversized_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("big.txt");
        std::fs::write(&file_path, b"1234567890").unwrap();
        let file_path_str = file_path.to_string_lossy().to_string();

        let fs = Fs::new(tmp.path());
        let result = check_file_size_limit(&fs, &file_path_str, 4).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains(&file_path_str),
            "error should mention file path: {msg}"
        );
    }

    /// P1：[check_file_size_limit] 文件大小等于限制时返回 Ok
    /// 条件：创建 10 字节文件，max_size = 10
    /// 断言：返回 Ok(())（等于限制不视为超限）
    #[tokio::test]
    async fn check_file_size_limit_equal_limit_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("exact.txt");
        std::fs::write(&file_path, b"1234567890").unwrap();
        let file_path_str = file_path.to_string_lossy().to_string();

        let fs = Fs::new(tmp.path());
        let result = check_file_size_limit(&fs, &file_path_str, 10).await;
        assert!(result.is_ok());
    }
}
