use std::collections::HashSet;

const MAX_DEPTH: usize = 64;

struct NodeInfo {
    name: Option<String>,
    ppid: Option<u32>,
}

enum ChainEnd {
    Root,
    Exited,
    Loop,
    DepthLimited,
}

struct ProcessChain {
    names: Vec<String>,
    end: ChainEnd,
}

impl ProcessChain {
    fn render(&self, max_len: Option<usize>) -> String {
        let mut parts: Vec<&str> = self.names.iter().map(|s| s.as_str()).collect();
        match self.end {
            ChainEnd::Root => {}
            ChainEnd::Exited => parts.push("(exited?)"),
            ChainEnd::Loop => parts.push("(loop-detected)"),
            ChainEnd::DepthLimited => parts.push("..."),
        }

        let Some(max) = max_len else {
            return parts.join(" < ");
        };

        let mut out = String::new();
        for (i, p) in parts.iter().enumerate() {
            let sep = if i == 0 { "" } else { " < " };
            if out.len() + sep.len() + p.len() > max {
                out.push_str(if out.is_empty() { "..." } else { " < ..." });
                break;
            }
            out.push_str(sep);
            out.push_str(p);
        }
        out
    }
}

fn build_chain<F>(start: u32, lookup: F) -> ProcessChain
where
    F: Fn(u32) -> Option<NodeInfo>,
{
    let mut names = Vec::new();
    let mut visited = HashSet::new();
    let mut cur = start;

    for _ in 0..MAX_DEPTH {
        let Some(info) = lookup(cur) else {
            return ProcessChain {
                names,
                end: ChainEnd::Exited,
            };
        };

        if !visited.insert(cur) {
            return ProcessChain {
                names,
                end: ChainEnd::Loop,
            };
        }

        names.push(info.name.unwrap_or_else(|| "[unknown]".to_string()));

        match info.ppid {
            None => {
                return ProcessChain {
                    names,
                    end: ChainEnd::Root,
                };
            }
            Some(ppid) if ppid == 0 || ppid == cur => {
                return ProcessChain {
                    names,
                    end: ChainEnd::Root,
                };
            }
            Some(ppid) => cur = ppid,
        }
    }

    ProcessChain {
        names,
        end: ChainEnd::DepthLimited,
    }
}

pub fn capture_current_capped(max_len: usize) -> String {
    build_chain(std::process::id(), platform::info).render(Some(max_len))
}

mod platform {
    use super::NodeInfo;

    #[cfg(target_os = "macos")]
    pub(super) fn info(pid: u32) -> Option<NodeInfo> {
        use std::mem;

        let mut bsd: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_bsdinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut bsd as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if n != size {
            return None;
        }

        let read_cstr = |ptr: *const libc::c_char| {
            let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            (!s.is_empty()).then_some(s)
        };
        let name = read_cstr(bsd.pbi_name.as_ptr()).or_else(|| read_cstr(bsd.pbi_comm.as_ptr()));
        Some(NodeInfo {
            name,
            ppid: Some(bsd.pbi_ppid),
        })
    }

    // SAFETY: 读取内核 procfs 的进程元数据（`/proc/<pid>/stat`、`/proc/<pid>/comm`），
    // 仅用于还原父进程链。路径由 pid 派生、非用户输入，且 /proc 不在任何沙箱根之内。
    #[allow(clippy::disallowed_methods)]
    #[cfg(target_os = "linux")]
    pub(super) fn info(pid: u32) -> Option<NodeInfo> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ppid = super::parse_ppid_from_stat(&stat)?;
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty());
        Some(NodeInfo {
            name,
            ppid: Some(ppid),
        })
    }

    #[cfg(target_os = "windows")]
    pub(super) fn info(pid: u32) -> Option<NodeInfo> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }

            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

            let mut result = None;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32ProcessID == pid {
                        let len = entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                        result = Some(NodeInfo {
                            name: (!name.is_empty()).then_some(name),
                            ppid: Some(entry.th32ParentProcessID),
                        });
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }

            CloseHandle(snapshot);
            result
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    pub(super) fn info(pid: u32) -> Option<NodeInfo> {
        (pid == std::process::id()).then(|| NodeInfo {
            name: std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned())),
            ppid: None,
        })
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_ppid_from_stat(stat: &str) -> Option<u32> {
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 1..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn map_lookup(
        m: HashMap<u32, (Option<&'static str>, Option<u32>)>,
    ) -> impl Fn(u32) -> Option<NodeInfo> {
        move |pid| {
            m.get(&pid).map(|(name, ppid)| NodeInfo {
                name: name.map(|s| s.to_string()),
                ppid: *ppid,
            })
        }
    }

    #[test]
    fn builds_chain_to_root() {
        let m = HashMap::from([(10, (Some("a"), Some(20))), (20, (Some("b"), Some(0)))]);
        let chain = build_chain(10, map_lookup(m));
        assert_eq!(chain.render(None), "a < b");
    }

    #[test]
    fn marks_exited_when_parent_missing() {
        let m = HashMap::from([(10, (Some("a"), Some(20)))]);
        let chain = build_chain(10, map_lookup(m));
        assert_eq!(chain.render(None), "a < (exited?)");
    }

    #[test]
    fn start_missing_yields_only_marker() {
        let chain = build_chain(99, map_lookup(HashMap::new()));
        assert_eq!(chain.render(None), "(exited?)");
    }

    #[test]
    fn detects_loop() {
        let m = HashMap::from([(10, (Some("a"), Some(20))), (20, (Some("b"), Some(10)))]);
        let chain = build_chain(10, map_lookup(m));
        assert_eq!(chain.render(None), "a < b < (loop-detected)");
    }

    #[test]
    fn self_reference_is_root() {
        let m = HashMap::from([(10, (Some("a"), Some(10)))]);
        let chain = build_chain(10, map_lookup(m));
        assert_eq!(chain.render(None), "a");
    }

    #[test]
    fn stops_at_max_depth() {
        let mut m = HashMap::new();
        for i in 0..(MAX_DEPTH as u32 + 10) {
            m.insert(i, (Some("p"), Some(i + 1)));
        }
        let chain = build_chain(0, map_lookup(m));
        assert!(chain.render(None).ends_with("..."));
    }

    #[test]
    fn node_without_name_renders_unknown() {
        let m = HashMap::from([(7, (None, None))]);
        let chain = build_chain(7, map_lookup(m));
        assert_eq!(chain.render(None), "[unknown]");
    }

    #[test]
    fn capped_truncates_at_node_boundary() {
        let m = HashMap::from([
            (1, (Some("aaaa"), Some(2))),
            (2, (Some("bbbb"), Some(3))),
            (3, (Some("cccc"), Some(4))),
            (4, (Some("dddd"), Some(0))),
        ]);
        let chain = build_chain(1, map_lookup(m));
        let out = chain.render(Some(14));
        assert!(out.starts_with("aaaa < bbbb"), "应保留近端: {out}");
        assert!(out.ends_with("..."), "应以省略号收尾: {out}");
        assert!(out.len() <= 14 + 6, "长度受控: {out}");
    }

    #[test]
    fn capture_current_capped_is_non_empty() {
        let text = capture_current_capped(512);
        assert!(!text.is_empty());
    }

    #[test]
    fn parse_ppid_basic() {
        let stat = "1234 (bash) S 1000 1234 1000 34816 1234 4194304 ...";
        assert_eq!(parse_ppid_from_stat(stat), Some(1000));
    }

    #[test]
    fn parse_ppid_with_tricky_comm() {
        let stat = "42 (weird )( name) S 7 42 7 0 -1 ...";
        assert_eq!(parse_ppid_from_stat(stat), Some(7));
    }

    #[test]
    fn parse_ppid_malformed() {
        assert_eq!(parse_ppid_from_stat("garbage-without-paren"), None);
        assert_eq!(parse_ppid_from_stat("1 (only-state) S"), None);
    }
}
