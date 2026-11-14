//! Kills everything an agent started. Killing only the `claude` process
//! leaves its tools' children (a `cargo build`, a dev server) running: the
//! CLI starts shell commands in their own session, and a background command
//! whose shell already exited has been reparented to init. So each agent
//! gets a marker variable in its environment, which every process it starts
//! inherits, and the reaper kills the processes carrying it plus anything
//! still below the agent in the process tree (Linux only: reads /proc).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MARKER_VAR: &str = "CLAUDECHAIN_RUN";

/// A value for [`MARKER_VAR`] that no other run of this or an earlier
/// ClaudeChain process has used.
pub fn new_marker() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, NEXT.fetch_add(1, Ordering::Relaxed))
}

/// SIGKILLs `root` (if given), its descendants and every process whose
/// environment carries `marker`. Each one is stopped first so it can't fork
/// new children between the scan and the kill.
#[cfg(target_os = "linux")]
pub fn kill_all(marker: &str, root: Option<u32>) {
    let needle = format!("{MARKER_VAR}={marker}");
    let me = std::process::id();
    let mut stopped: HashSet<u32> = HashSet::new();
    if let Some(root) = root.filter(|&p| p != 0) {
        signal(root, libc::SIGSTOP);
        stopped.insert(root);
    }
    // Rescan until a pass finds nothing new: a process may have forked
    // just before it was stopped.
    for _ in 0..20 {
        let procs = read_ppids();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, &ppid) in &procs {
            children.entry(ppid).or_default().push(pid);
        }
        let mut found: Vec<u32> = procs
            .keys()
            .copied()
            .filter(|&pid| pid != me && !stopped.contains(&pid) && has_marker(pid, &needle))
            .collect();
        let mut queue: Vec<u32> = stopped.iter().copied().collect();
        while let Some(pid) = queue.pop() {
            for &c in children.get(&pid).into_iter().flatten() {
                if c != me && !stopped.contains(&c) && !found.contains(&c) {
                    found.push(c);
                    queue.push(c);
                }
            }
        }
        if found.is_empty() {
            break;
        }
        for pid in found {
            signal(pid, libc::SIGSTOP);
            stopped.insert(pid);
        }
    }
    for pid in stopped {
        signal(pid, libc::SIGKILL);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn kill_all(_marker: &str, _root: Option<u32>) {}

#[cfg(target_os = "linux")]
fn signal(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(target_os = "linux")]
fn has_marker(pid: u32, needle: &str) -> bool {
    // Only our own processes' environments are readable, which is also the
    // set we may signal.
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|env| env.split(|&b| b == 0).any(|var| var == needle.as_bytes()))
        .unwrap_or(false)
}

/// pid → parent pid for every live process.
#[cfg(target_os = "linux")]
fn read_ppids() -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // "pid (comm) state ppid ..."; comm may contain spaces and parens.
        let Some(close) = stat.rfind(')') else { continue };
        let Some(ppid) = stat[close + 1..].split_whitespace().nth(1).and_then(|v| v.parse().ok())
        else {
            continue;
        };
        out.insert(pid, ppid);
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    fn alive(pid: u32) -> bool {
        // A killed child that hasn't been reaped yet is a zombie.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| s.rfind(')').is_some_and(|i| !s[i + 1..].trim_start().starts_with('Z')))
            .unwrap_or(false)
    }

    #[test]
    fn kills_detached_and_orphaned_descendants() {
        let marker = new_marker();
        let dir = std::env::temp_dir().join(format!("reaper-test-{marker}"));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pids");
        // A shell that starts one grandchild in a new session and one it
        // abandons (the subshell exits, so it is reparented away from us).
        let script = format!(
            "setsid sleep 300 & echo $! > {p}; (sleep 300 & echo $! >> {p}) ; sleep 300",
            p = pidfile.display()
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env(MARKER_VAR, &marker)
            .spawn()
            .unwrap();
        let mut pids = vec![];
        for _ in 0..100 {
            pids = std::fs::read_to_string(&pidfile)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect();
            if pids.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(pids.len(), 2);
        assert!(pids.iter().all(|&p| alive(p)));

        kill_all(&marker, Some(child.id()));
        child.wait().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        for p in pids {
            assert!(!alive(p), "{p} survived");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
