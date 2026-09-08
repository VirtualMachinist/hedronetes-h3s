//! Observed Linux capacity bounded by the agent's cgroup ancestry.
use serde_json::{json, Value};
use std::{fs, path::Path};

/// Keep a small explicit reservation for the OS/runtime. Missing or malformed
/// capacity yields no allocatable report; a scheduler must not infer zero usage.
pub fn observe() -> Option<(Value, Value)> {
    let cpu = i64::try_from(std::thread::available_parallelism().ok()?.get())
        .ok()?
        .checked_mul(1000)?;
    let memory = fs::read_to_string("/proc/meminfo").ok()?;
    let memory = memory.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let mut fields = memory.split_whitespace();
    let memory = fields.next()?.parse::<i64>().ok()?.checked_mul(1024)?;
    if fields.next() != Some("kB") {
        return None;
    }
    let membership = fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = membership.lines().find_map(|l| l.strip_prefix("0::/"))?;
    if relative.split('/').any(|part| matches!(part, "." | "..")) {
        return None;
    }
    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").is_file() {
        return None;
    }
    let mut path = root.join(relative);
    let (mut cpu, mut memory) = (cpu, memory);
    loop {
        cpu = bound_cpu(cpu, read_optional(&path.join("cpu.max"))?.as_deref())?;
        memory = bound_memory(memory, read_optional(&path.join("memory.max"))?.as_deref())?;
        if path == root {
            break;
        }
        if !path.pop() || !path.starts_with(root) {
            return None;
        }
    }
    if cpu <= 100 || memory <= 256 * 1024 * 1024 {
        return None;
    }
    Some((
        json!({"cpu":format!("{cpu}m"),"memory":memory.to_string(),"pods":"110"}),
        json!({"cpu":format!("{}m",cpu-100),"memory":(memory-256*1024*1024).to_string(),"pods":"110"}),
    ))
}
fn read_optional(path: &Path) -> Option<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Some(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}
fn bound_memory(current: i64, value: Option<&str>) -> Option<i64> {
    match value.map(str::trim) {
        None | Some("max") => Some(current),
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|v| *v > 0)
            .map(|v| current.min(v)),
    }
}
fn bound_cpu(current: i64, value: Option<&str>) -> Option<i64> {
    let Some(value) = value else {
        return Some(current);
    };
    let mut fields = value.split_whitespace();
    let quota = fields.next()?;
    let period = fields.next()?.parse::<i64>().ok().filter(|v| *v > 0)?;
    if fields.next().is_some() {
        return None;
    }
    if quota == "max" {
        return Some(current);
    }
    let quota = quota.parse::<i64>().ok().filter(|v| *v > 0)?;
    // Capacity rounds down; requested CPU rounds up. Never advertise excess.
    Some(current.min(quota.checked_mul(1000)? / period))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hierarchical_limits_and_invalid_reports() {
        assert_eq!(bound_cpu(4000, Some("150000 100000")), Some(1500));
        assert_eq!(bound_cpu(1500, Some("200000 100000")), Some(1500));
        assert_eq!(bound_cpu(1500, Some("max 100000")), Some(1500));
        assert_eq!(bound_cpu(4000, Some("1 3000")), Some(0));
        assert_eq!(bound_memory(1024, Some("512")), Some(512));
        assert_eq!(bound_memory(512, Some("1024")), Some(512));
        assert_eq!(bound_memory(512, Some("max")), Some(512));
        for s in ["-1 100000", "1 0", "max", "garbage", "1 1 extra"] {
            assert_eq!(bound_cpu(1000, Some(s)), None);
        }
        for s in ["-1", "0", "garbage"] {
            assert_eq!(bound_memory(512, Some(s)), None);
        }
    }
}
