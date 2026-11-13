//! Resolve only a Devin ACP child owned by this registered TUI process.
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Root {
    pid: u32,
    start: Option<u64>,
    pub(crate) cwd: PathBuf,
    group: String,
    actor: String,
    pub(crate) generation: String,
}

#[derive(Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Binding {
    pub(crate) directory: PathBuf,
    pub(crate) session: String,
    pid: u32,
    start: u64,
    lock_modified: SystemTime,
    lock_directory_modified: SystemTime,
}

impl Root {
    pub(crate) fn new(
        pid: u32,
        cwd: PathBuf,
        group: String,
        actor: String,
        generation: String,
    ) -> Self {
        Self {
            pid,
            start: process_start(pid),
            cwd,
            group,
            actor,
            generation,
        }
    }

    pub(crate) fn bind(&self) -> Option<Binding> {
        #[cfg(target_os = "linux")]
        return linux::bind(self);
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

impl Binding {
    #[cfg(test)]
    pub(crate) fn fixture(directory: PathBuf, session: String) -> Self {
        Self {
            directory,
            session,
            pid: 0,
            start: 0,
            lock_modified: SystemTime::UNIX_EPOCH,
            lock_directory_modified: SystemTime::UNIX_EPOCH,
        }
    }

    pub(crate) fn current(&self, root: &Root) -> bool {
        #[cfg(target_os = "linux")]
        return linux::current(self, root).unwrap_or(false);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            false
        }
    }
}

fn process_start(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    return linux::start(pid);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::process_tree::linux_identity::ProcView;
    use std::collections::VecDeque;
    use std::fs::{self, File};
    use std::io::Read;

    pub(super) fn start(pid: u32) -> Option<u64> {
        let row = ProcView::open()
            .ok()?
            .process(i32::try_from(pid).ok()?)
            .ok()?;
        (!matches!(row.state, 'Z' | 'X')).then_some(row.start_ticks)
    }

    fn read(path: impl AsRef<Path>, max: u64) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(max + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() as u64 <= max).then_some(bytes)
    }

    fn text(path: impl AsRef<Path>, max: u64) -> Option<String> {
        String::from_utf8(read(path, max)?).ok()
    }

    fn parent(pid: u32) -> Option<u32> {
        let status = text(format!("/proc/{pid}/status"), 16 * 1024)?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))?
            .trim()
            .parse()
            .ok()
    }

    fn owned(root: &Root, pid: u32) -> bool {
        if root.start.is_none() || start(root.pid) != root.start {
            return false;
        }
        let mut cursor = pid;
        for _ in 0..4 {
            if cursor == root.pid {
                return true;
            }
            let Some(next) = parent(cursor) else {
                return false;
            };
            cursor = next;
        }
        false
    }

    fn native_directory(root: &Root, pid: u32) -> Option<PathBuf> {
        let command = read(format!("/proc/{pid}/cmdline"), 16 * 1024)?;
        let args = command.split(|b| *b == 0).collect::<Vec<_>>();
        let program = std::str::from_utf8(args.first()?).ok()?;
        if Path::new(program).file_name()? != "devin" || !args.contains(&b"acp".as_slice()) {
            return None;
        }
        // Read no environment into diagnostics or state. Retain only attribution/home.
        let bytes = read(format!("/proc/{pid}/environ"), 256 * 1024)?;
        let value = |name: &str| -> Option<&str> {
            bytes
                .split(|b| *b == 0)
                .filter_map(|part| std::str::from_utf8(part).ok())
                .find_map(|part| part.strip_prefix(name))
        };
        if value("CCCC_ACTOR_ID=")? != root.actor || value("CCCC_GROUP_ID=")? != root.group {
            return None;
        }
        let data = match value("XDG_DATA_HOME=").filter(|s| !s.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(value("HOME=")?).join(".local/share"),
        };
        if !data.is_absolute() {
            return None;
        }
        data.join("devin/cli").canonicalize().ok()
    }

    fn seconds(ticks: u64) -> Option<i64> {
        let boot = text("/proc/stat", 64 * 1024)?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .parse::<u64>()
            .ok()?;
        let hz = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK).ok()??;
        let hz = u64::try_from(hz).ok()?.max(1);
        i64::try_from(boot.checked_add(ticks / hz)?).ok()
    }

    pub(super) fn bind(root: &Root) -> Option<Binding> {
        let mut queue = VecDeque::from([(root.pid, 0)]);
        let mut found = None;
        let mut visited = 0;
        while let Some((pid, depth)) = queue.pop_front() {
            visited += 1;
            if visited > 64 {
                return None;
            }
            if let Some(directory) = native_directory(root, pid) {
                let start = start(pid)?;
                let started_seconds = seconds(start)?;
                let locks = directory.join("session_locks");
                let lock_directory_modified = fs::metadata(&locks).ok()?.modified().ok()?;
                for (index, entry) in fs::read_dir(&locks).ok()?.enumerate() {
                    if index >= 32768 {
                        return None;
                    }
                    let entry = entry.ok()?;
                    if entry.path().extension().is_none_or(|ext| ext != "lock") {
                        continue;
                    }
                    if text(entry.path(), 32)?.trim().parse::<u32>().ok()? != pid {
                        continue;
                    }
                    let session = entry.path().file_stem()?.to_str()?.to_owned();
                    if session.is_empty()
                        || !session
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    {
                        return None;
                    }
                    let lock_modified = entry.metadata().ok()?.modified().ok()?;
                    let modified = i64::try_from(
                        lock_modified
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()?
                            .as_secs(),
                    )
                    .ok()?;
                    if modified < started_seconds.saturating_sub(3) {
                        continue;
                    }
                    let binding = Binding {
                        directory: directory.clone(),
                        session,
                        pid,
                        start,
                        lock_modified,
                        lock_directory_modified,
                    };
                    if !current(&binding, root)? {
                        return None;
                    }
                    if found.is_some() {
                        return None;
                    }
                    found = Some(binding);
                }
                // A new lock could make a cached binding ambiguous. Directory
                // changes force a complete rebind and retire its old input fence.
                if fs::metadata(locks).ok()?.modified().ok()? != lock_directory_modified {
                    return None;
                }
            }
            if depth < 3 {
                let children = text(format!("/proc/{pid}/task/{pid}/children"), 4096)?;
                for child in children.split_whitespace() {
                    queue.push_back((child.parse::<u32>().ok()?, depth + 1));
                }
            }
        }
        found
    }

    pub(super) fn current(binding: &Binding, root: &Root) -> Option<bool> {
        if !owned(root, binding.pid)
            || start(binding.pid) != Some(binding.start)
            || native_directory(root, binding.pid)? != binding.directory
            || fs::metadata(binding.directory.join("session_locks"))
                .ok()?
                .modified()
                .ok()?
                != binding.lock_directory_modified
        {
            return Some(false);
        }
        let lock = binding
            .directory
            .join("session_locks")
            .join(format!("{}.lock", binding.session));
        Some(
            text(&lock, 32)?.trim().parse::<u32>().ok()? == binding.pid
                && fs::metadata(lock).ok()?.modified().ok()? == binding.lock_modified,
        )
    }
}
