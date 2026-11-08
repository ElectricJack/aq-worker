//! Parsers for `/proc/stat` and `/proc/meminfo`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
    pub logical_cpus: u32,
}

pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let mut agg: Option<(u64, u64)> = None;
    let mut cpus = 0u32;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            let v: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if v.len() < 8 {
                return None;
            }
            let total: u64 = v[..8].iter().sum();
            agg = Some((total - (v[3] + v[4]), total));
        } else if let Some(rest) = line.strip_prefix("cpu")
            && rest.as_bytes().first().is_some_and(u8::is_ascii_digit)
        {
            cpus += 1;
        }
    }
    let (busy, total) = agg?;
    Some(CpuTimes {
        busy,
        total,
        logical_cpus: cpus,
    })
}

pub fn cpu_busy_bps(prev: &CpuTimes, cur: &CpuTimes) -> u16 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0;
    }
    let db = cur.busy.saturating_sub(prev.busy);
    (db.saturating_mul(10000) / dt).min(10000) as u16
}

/// Total and available memory in bytes. Both lines are required.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let (mut total, mut avail) = (None, None);
    for line in text.lines() {
        let kb = |rest: &str| -> Option<u64> {
            rest.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()?
                .checked_mul(1024)
        };
        if let Some(r) = line.strip_prefix("MemTotal:") {
            total = kb(r);
        } else if let Some(r) = line.strip_prefix("MemAvailable:") {
            avail = kb(r);
        }
    }
    Some((total?, avail?))
}
