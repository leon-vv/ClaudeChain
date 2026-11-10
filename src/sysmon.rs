//! System resource sampling for the Resources page (Linux only: reads /proc
//! and /sys). A background task takes a sample every few seconds and keeps a
//! short history; on other systems the samples stay empty.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;

pub const INTERVAL: Duration = Duration::from_secs(2);
const HISTORY: usize = 150; // 5 minutes

#[derive(Debug, Clone, Serialize, Default)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sensor {
    pub name: String,
    pub celsius: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Disk {
    pub path: PathBuf,
    pub total: u64,
    pub free: u64,
}

/// CPU and memory of one process tree (an agent plus everything it started).
#[derive(Debug, Clone, Serialize)]
pub struct TreeUsage {
    pub root: u32,
    pub processes: usize,
    pub cpu: f64,
    pub rss: u64,
    /// Names of the busiest processes in the tree, e.g. "cargo", "rustc".
    pub top: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Sample {
    pub at: Option<DateTime<Utc>>,
    pub supported: bool,
    /// Busy percentage, overall and per core.
    pub cpu: f64,
    pub cores: Vec<f64>,
    pub load: [f64; 3],
    pub memory: Memory,
    pub sensors: Vec<Sensor>,
    /// Highest CPU package/core temperature, when a CPU sensor is found.
    pub cpu_temp: Option<f64>,
    pub disk: Option<Disk>,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Point {
    pub at: DateTime<Utc>,
    pub cpu: f64,
    pub mem_used: u64,
    pub cpu_temp: Option<f64>,
}

#[derive(Default)]
struct Prev {
    cpu: Vec<(u64, u64)>,
    procs: HashMap<u32, u64>,
    at: Option<Instant>,
}

pub struct Monitor {
    prev: Mutex<Prev>,
    latest: Mutex<Sample>,
    history: Mutex<VecDeque<Point>>,
    /// Per-process CPU% from the latest sample, for tree usage.
    proc_cpu: Mutex<HashMap<u32, (u32, f64, u64, String)>>,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            prev: Mutex::new(Prev::default()),
            latest: Mutex::new(Sample::default()),
            history: Mutex::new(VecDeque::new()),
            proc_cpu: Mutex::new(HashMap::new()),
        }
    }

    pub fn latest(&self) -> Sample {
        self.latest.lock().unwrap().clone()
    }

    pub fn history(&self) -> Vec<Point> {
        self.history.lock().unwrap().iter().cloned().collect()
    }

    /// Takes one sample. `disk_path` is the directory whose filesystem to report.
    pub fn sample(&self, disk_path: Option<&Path>) {
        if !cfg!(target_os = "linux") || !Path::new("/proc/stat").exists() {
            return;
        }
        let now = Instant::now();
        let mut prev = self.prev.lock().unwrap();
        let elapsed = prev.at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);

        let times = read_cpu_times();
        let busy = |i: usize| -> f64 {
            match (times.get(i), prev.cpu.get(i)) {
                (Some(&(b, t)), Some(&(pb, pt))) if t > pt => (b.saturating_sub(pb)) as f64 * 100.0 / (t - pt) as f64,
                _ => 0.0,
            }
        };
        let cpu = busy(0);
        let cores: Vec<f64> = (1..times.len()).map(busy).collect();

        // Per-process CPU, for agent process trees.
        let ticks = clock_ticks();
        let procs = read_procs();
        let mut proc_cpu = HashMap::new();
        for p in &procs {
            let pct = match prev.procs.get(&p.pid) {
                Some(&before) if elapsed > 0.0 => p.cpu_ticks.saturating_sub(before) as f64 * 100.0 / (ticks * elapsed),
                _ => 0.0,
            };
            proc_cpu.insert(p.pid, (p.ppid, pct, p.rss, p.name.clone()));
        }
        prev.procs = procs.iter().map(|p| (p.pid, p.cpu_ticks)).collect();
        prev.cpu = times;
        prev.at = Some(now);
        drop(prev);
        *self.proc_cpu.lock().unwrap() = proc_cpu;

        let sensors = read_sensors(Path::new("/sys/class"));
        let cpu_temp = sensors
            .iter()
            .filter(|s| is_cpu_sensor(&s.name))
            .map(|s| s.celsius)
            .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
        let memory = read_memory();
        let sample = Sample {
            at: Some(Utc::now()),
            supported: true,
            cpu,
            cores,
            load: read_load(),
            sensors,
            cpu_temp,
            disk: disk_path.and_then(read_disk),
            uptime_secs: read_uptime(),
            memory: memory.clone(),
        };
        if elapsed > 0.0 {
            let mut h = self.history.lock().unwrap();
            h.push_back(Point {
                at: Utc::now(),
                cpu,
                mem_used: memory.total.saturating_sub(memory.available),
                cpu_temp,
            });
            while h.len() > HISTORY {
                h.pop_front();
            }
        }
        *self.latest.lock().unwrap() = sample;
    }

    /// CPU and memory of `root` and all its descendants, from the latest sample.
    pub fn tree(&self, root: u32) -> Option<TreeUsage> {
        let procs = self.proc_cpu.lock().unwrap();
        procs.get(&root)?;
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, &(ppid, ..)) in procs.iter() {
            children.entry(ppid).or_default().push(pid);
        }
        let mut stack = vec![root];
        let mut members = vec![];
        while let Some(pid) = stack.pop() {
            members.push(pid);
            if let Some(c) = children.get(&pid) {
                stack.extend(c);
            }
        }
        let mut busiest: Vec<(f64, String)> = vec![];
        let (mut cpu, mut rss) = (0.0, 0);
        for pid in &members {
            if let Some((_, pct, r, name)) = procs.get(pid) {
                cpu += pct;
                rss += r;
                busiest.push((*pct, name.clone()));
            }
        }
        busiest.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut top: Vec<String> = vec![];
        for (pct, name) in busiest {
            if pct >= 1.0 && !top.contains(&name) && top.len() < 3 {
                top.push(name);
            }
        }
        Some(TreeUsage { root, processes: members.len(), cpu, rss, top })
    }
}

fn is_cpu_sensor(name: &str) -> bool {
    let n = name.to_lowercase();
    ["coretemp", "k10temp", "zenpower", "cpu", "x86_pkg_temp", "soc_thermal"].iter().any(|k| n.contains(k))
}

/// (busy, total) jiffies for the "cpu" line and each "cpuN" line.
fn read_cpu_times() -> Vec<(u64, u64)> {
    let Ok(text) = std::fs::read_to_string("/proc/stat") else { return vec![] };
    text.lines()
        .filter(|l| l.starts_with("cpu"))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total.saturating_sub(idle), total)
        })
        .collect()
}

fn read_memory() -> Memory {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else { return Memory::default() };
    let get = |key: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    Memory {
        total: get("MemTotal:"),
        available: get("MemAvailable:"),
        swap_total: get("SwapTotal:"),
        swap_free: get("SwapFree:"),
    }
}

fn read_load() -> [f64; 3] {
    let text = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let v: Vec<f64> = text.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
    [v.first().copied().unwrap_or(0.0), v.get(1).copied().unwrap_or(0.0), v.get(2).copied().unwrap_or(0.0)]
}

fn read_uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .map_or(0, |v| v as u64)
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Temperatures from hwmon (with labels), falling back to thermal zones.
/// `class` is normally /sys/class.
fn read_sensors(class: &Path) -> Vec<Sensor> {
    let mut out = vec![];
    if let Ok(dirs) = std::fs::read_dir(class.join("hwmon")) {
        for dir in dirs.flatten() {
            let dir = dir.path();
            let chip = read_trim(&dir.join("name")).unwrap_or_else(|| "hwmon".into());
            let Ok(files) = std::fs::read_dir(&dir) else { continue };
            let mut inputs: Vec<PathBuf> = files
                .flatten()
                .map(|f| f.path())
                .filter(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy();
                    n.starts_with("temp") && n.ends_with("_input")
                })
                .collect();
            inputs.sort();
            for input in inputs {
                let Some(milli) = read_trim(&input).and_then(|v| v.parse::<f64>().ok()) else { continue };
                let label_path = PathBuf::from(input.to_string_lossy().replace("_input", "_label"));
                let name = match read_trim(&label_path) {
                    Some(label) => format!("{chip} {label}"),
                    None => chip.clone(),
                };
                out.push(Sensor { name, celsius: milli / 1000.0 });
            }
        }
    }
    if out.is_empty()
        && let Ok(zones) = std::fs::read_dir(class.join("thermal"))
    {
        for zone in zones.flatten() {
            let p = zone.path();
            if !p.file_name().unwrap_or_default().to_string_lossy().starts_with("thermal_zone") {
                continue;
            }
            let (Some(kind), Some(milli)) = (read_trim(&p.join("type")), read_trim(&p.join("temp"))) else { continue };
            if let Ok(m) = milli.parse::<f64>() {
                out.push(Sensor { name: kind, celsius: m / 1000.0 });
            }
        }
    }
    out.retain(|s| s.celsius > -50.0 && s.celsius < 150.0);
    out
}

fn read_disk(path: &Path) -> Option<Disk> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(Disk {
        path: path.to_path_buf(),
        total: st.f_blocks as u64 * frsize,
        free: st.f_bavail as u64 * frsize,
    })
}

fn clock_ticks() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

struct Proc {
    pid: u32,
    ppid: u32,
    cpu_ticks: u64,
    rss: u64,
    name: String,
}

fn read_procs() -> Vec<Proc> {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let Ok(entries) = std::fs::read_dir("/proc") else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // "pid (comm) state ppid ..."; comm may contain spaces and parens.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
        let name = stat[open + 1..close].to_string();
        let rest: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        // Fields after comm: state(0) ppid(1) ... utime(11) stime(12) ... rss(21)
        let field = |i: usize| rest.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        out.push(Proc {
            pid,
            ppid: field(1) as u32,
            cpu_ticks: field(11) + field(12),
            rss: field(21) * page,
            name,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn sensors_from_hwmon_and_thermal_zones() {
        let root = std::env::temp_dir().join(format!("claudechain-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // hwmon with labels takes precedence.
        write(&root.join("hwmon/hwmon0/name"), "coretemp\n");
        write(&root.join("hwmon/hwmon0/temp1_input"), "61000\n");
        write(&root.join("hwmon/hwmon0/temp1_label"), "Package id 0\n");
        write(&root.join("hwmon/hwmon0/temp2_input"), "57500\n");
        write(&root.join("hwmon/hwmon1/name"), "nvme\n");
        write(&root.join("hwmon/hwmon1/temp1_input"), "40850\n");
        let s = read_sensors(&root);
        let names: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.celsius)).collect();
        assert!(names.contains(&("coretemp Package id 0", 61.0)));
        assert!(names.contains(&("coretemp", 57.5)));
        assert!(names.contains(&("nvme", 40.85)));
        assert_eq!(s.iter().filter(|s| is_cpu_sensor(&s.name)).count(), 2);

        // Without hwmon, thermal zones are used.
        std::fs::remove_dir_all(root.join("hwmon")).unwrap();
        write(&root.join("thermal/thermal_zone0/type"), "x86_pkg_temp\n");
        write(&root.join("thermal/thermal_zone0/temp"), "48000\n");
        write(&root.join("thermal/cooling_device0/type"), "Processor\n");
        let s = read_sensors(&root);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].name.as_str(), s[0].celsius), ("x86_pkg_temp", 48.0));
        assert!(is_cpu_sensor(&s[0].name));
        let _ = std::fs::remove_dir_all(&root);
    }
}
