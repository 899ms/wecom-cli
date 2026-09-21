use std::collections::HashSet;

const MAX_DEPTH: usize = 64;
const DSH_ENV_MARKERS: [(&str, &str); 3] = [
    ("DSH_SHELL", "dsh-shell"),
    ("DSH_SESSION_ID", "dsh-session"),
    ("DSH_HOME", "dsh-home"),
];

struct NodeInfo {
    name: Option<String>,
    ppid: Option<u32>,
    path: Option<String>,
}

struct ProcessNode {
    name: String,
    path: Option<String>,
}

enum ChainEnd {
    Root,
    Exited,
    Loop,
    DepthLimited,
}

struct ProcessChain {
    nodes: Vec<ProcessNode>,
    end: ChainEnd,
}

impl ProcessChain {
    #[cfg(test)]
    fn render(&self, max_len: Option<usize>) -> String {
        self.render_with_env(max_len, |_| false)
    }

    fn render_with_env<F>(&self, max_len: Option<usize>, env_exists: F) -> String
    where
        F: FnMut(&str) -> bool,
    {
        let mut names: Vec<String> = self.nodes.iter().map(|node| node.name.clone()).collect();
        if cfg!(target_os = "macos") {
            decorate_macos_process_names(&mut names, &self.nodes);
        }
        decorate_current_process_name(&mut names, env_exists);

        let mut parts: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
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

fn decorate_current_process_name<F>(names: &mut [String], mut env_exists: F)
where
    F: FnMut(&str) -> bool,
{
    let Some(name) = names.first_mut() else {
        return;
    };

    let markers = DSH_ENV_MARKERS
        .into_iter()
        .filter_map(|(env_name, marker)| env_exists(env_name).then_some(marker))
        .collect::<Vec<_>>();
    if !markers.is_empty() {
        name.push('[');
        name.push_str(&markers.join(","));
        name.push(']');
    }
}

fn decorate_macos_process_names(names: &mut [String], nodes: &[ProcessNode]) {
    let app_names: Vec<Option<String>> = nodes
        .iter()
        .map(|node| node.path.as_deref().and_then(app_bundle_names))
        .collect();

    if app_names.iter().any(Option::is_some) {
        for (name, app_names) in names.iter_mut().zip(app_names) {
            if let Some(app_names) = app_names {
                name.push('[');
                name.push_str(&app_names);
                name.push(']');
            }
        }
        return;
    }

    if let Some((index, path)) = nodes.iter().enumerate().rev().find_map(|(index, node)| {
        node.path
            .as_deref()
            .map(|path| (index, redact_macos_username(path)))
    }) {
        names[index].push_str("[path=");
        names[index].push_str(&path);
        names[index].push(']');
    }
}

fn app_bundle_names(path: &str) -> Option<String> {
    let names = std::path::Path::new(path)
        .components()
        .filter_map(|component| {
            let component = component.as_os_str().to_string_lossy();
            (component.len() > ".app".len() && component.ends_with(".app"))
                .then(|| component.into_owned())
        })
        .collect::<Vec<_>>();

    (!names.is_empty()).then(|| names.join(" - "))
}

fn redact_macos_username(path: &str) -> String {
    const USERS_PREFIX: &str = "/users/";

    let Some(prefix) = path.get(..USERS_PREFIX.len()) else {
        return path.to_string();
    };
    if !prefix.eq_ignore_ascii_case(USERS_PREFIX) {
        return path.to_string();
    }

    let rest = &path[USERS_PREFIX.len()..];
    match rest.find('/') {
        Some(end) if end > 0 => format!("{}*{}", &path[..USERS_PREFIX.len()], &rest[end..]),
        None if !rest.is_empty() => format!("{}*", &path[..USERS_PREFIX.len()]),
        _ => path.to_string(),
    }
}

fn build_chain<F>(start: u32, lookup: F) -> ProcessChain
where
    F: Fn(u32) -> Option<NodeInfo>,
{
    let mut nodes = Vec::new();
    let mut visited = HashSet::new();
    let mut cur = start;

    for _ in 0..MAX_DEPTH {
        let Some(info) = lookup(cur) else {
            return ProcessChain {
                nodes,
                end: ChainEnd::Exited,
            };
        };

        if !visited.insert(cur) {
            return ProcessChain {
                nodes,
                end: ChainEnd::Loop,
            };
        }

        let NodeInfo { name, ppid, path } = info;
        nodes.push(ProcessNode {
            name: name.unwrap_or_else(|| "[unknown]".to_string()),
            path,
        });

        match ppid {
            None => {
                return ProcessChain {
                    nodes,
                    end: ChainEnd::Root,
                };
            }
            Some(ppid) if ppid == 0 || ppid == cur => {
                return ProcessChain {
                    nodes,
                    end: ChainEnd::Root,
                };
            }
            Some(ppid) => cur = ppid,
        }
    }

    ProcessChain {
        nodes,
        end: ChainEnd::DepthLimited,
    }
}

pub fn capture_current_capped(max_len: usize) -> String {
    build_chain(std::process::id(), platform::info)
        .render_with_env(Some(max_len), |name| std::env::var_os(name).is_some())
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
            path: process_path(pid),
        })
    }

    #[cfg(target_os = "macos")]
    fn process_path(pid: u32) -> Option<String> {
        let mut buffer = [0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let size = unsafe {
            libc::proc_pidpath(
                pid as i32,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len() as u32,
            )
        };
        if size <= 0 {
            return None;
        }

        let bytes = &buffer[..size as usize];
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        let path = String::from_utf8_lossy(&bytes[..end]).into_owned();
        (!path.is_empty()).then_some(path)
    }

    // /proc 是内核虚拟文件系统而非用户文件，不纳入 Fs 沙箱（沙箱根不可能包含
    // /proc，经 Fs 读取只会被沙箱拒绝）；pid 来自 OS 进程父链而非用户输入，
    // 路径无注入面，属正当绕过。
    #[cfg(target_os = "linux")]
    #[allow(clippy::disallowed_methods)]
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
            path: None,
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
                            path: None,
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
            path: None,
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
                path: None,
            })
        }
    }

    fn node(name: &str, path: &str) -> ProcessNode {
        ProcessNode {
            name: name.to_string(),
            path: Some(path.to_string()),
        }
    }

    #[test]
    fn dsh_env_markers_cover_all_combinations() {
        let chain = build_chain(
            10,
            map_lookup(HashMap::from([
                (10, (Some("wecom-cli"), Some(20))),
                (20, (Some("parent"), Some(0))),
            ])),
        );
        let cases: &[(&[&str], &str)] = &[
            (&[], "wecom-cli < parent"),
            (&["DSH_SHELL"], "wecom-cli[dsh-shell] < parent"),
            (&["DSH_SESSION_ID"], "wecom-cli[dsh-session] < parent"),
            (&["DSH_HOME"], "wecom-cli[dsh-home] < parent"),
            (
                &["DSH_SHELL", "DSH_SESSION_ID"],
                "wecom-cli[dsh-shell,dsh-session] < parent",
            ),
            (
                &["DSH_SHELL", "DSH_HOME"],
                "wecom-cli[dsh-shell,dsh-home] < parent",
            ),
            (
                &["DSH_SESSION_ID", "DSH_HOME"],
                "wecom-cli[dsh-session,dsh-home] < parent",
            ),
            (
                &["DSH_SHELL", "DSH_SESSION_ID", "DSH_HOME"],
                "wecom-cli[dsh-shell,dsh-session,dsh-home] < parent",
            ),
        ];

        for &(present, expected) in cases {
            assert_eq!(
                chain.render_with_env(None, |name| present.contains(&name)),
                expected
            );
        }
    }

    #[test]
    fn dsh_env_markers_only_decorate_current_process() {
        let mut names = vec!["wecom-cli".to_string(), "parent".to_string()];

        decorate_current_process_name(&mut names, |_| true);

        assert_eq!(
            names,
            ["wecom-cli[dsh-shell,dsh-session,dsh-home]", "parent"]
        );
    }

    #[test]
    fn macos_path_collects_all_app_bundle_names() {
        assert_eq!(
            app_bundle_names(
                "/Applications/MyApp.app/Contents/Frameworks/Electron.app/Contents/MacOS/Electron"
            ),
            Some("MyApp.app - Electron.app".to_string())
        );
        assert_eq!(app_bundle_names("/usr/local/bin/node"), None);
    }

    #[test]
    fn macos_decorates_each_process_with_all_app_bundle_names() {
        let nodes = vec![
            node(
                "Electron",
                "/Applications/MyApp.app/Contents/Frameworks/Electron.app/Contents/MacOS/Electron",
            ),
            node("helper", "/Applications/Helper.app/Contents/MacOS/helper"),
            node("launchd", "/sbin/launchd"),
        ];
        let mut names = nodes
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>();

        decorate_macos_process_names(&mut names, &nodes);

        assert_eq!(
            names,
            [
                "Electron[MyApp.app - Electron.app]",
                "helper[Helper.app]",
                "launchd"
            ]
        );
    }

    #[test]
    fn macos_without_app_reports_users_path_with_redacted_username() {
        let nodes = vec![
            node("wecom-cli", "/opt/wecom/bin/wecom-cli"),
            node("node", "/usr/local/bin/node"),
            node("Electron", "/Users/alice/tools/Electron"),
        ];
        let mut names = nodes
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>();

        decorate_macos_process_names(&mut names, &nodes);

        assert_eq!(
            names,
            [
                "wecom-cli",
                "node",
                "Electron[path=/Users/*/tools/Electron]"
            ]
        );
    }

    #[test]
    fn macos_users_path_redacts_only_username() {
        assert_eq!(
            redact_macos_username("/Users/alice/workspace/project/bin"),
            "/Users/*/workspace/project/bin"
        );
        assert_eq!(
            redact_macos_username("/users/alice/Electron"),
            "/users/*/Electron"
        );
        assert_eq!(redact_macos_username("/Users/alice"), "/Users/*");
        assert_eq!(redact_macos_username("/Users/"), "/Users/");
        assert_eq!(
            redact_macos_username("/UsersBackup/alice/Electron"),
            "/UsersBackup/alice/Electron"
        );
    }

    #[test]
    fn macos_without_app_reports_non_users_full_path_unchanged() {
        let nodes = vec![
            node("wecom-cli", "/opt/wecom/bin/wecom-cli"),
            node("Electron", "/opt/agent/bin/Electron"),
        ];
        let mut names = nodes
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>();

        decorate_macos_process_names(&mut names, &nodes);

        assert_eq!(
            names,
            ["wecom-cli", "Electron[path=/opt/agent/bin/Electron]"]
        );
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
