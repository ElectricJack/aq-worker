use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::host::{HOST_FRESH_MS, HostLoad, read_host_load};
use crate::proc::{CpuTimes, cpu_busy_bps, parse_meminfo, parse_proc_stat};
use crate::{HostProbe, LoadSource, MachineConfig, MachineReport, Sampler};

const HOST_FILE: &str = r#"{"schema":"aq-host-load-1","observed_at_ms":1000,"logical_cpus":24,"cpu_busy_bps":6100,"memory_total_bytes":100,"memory_used_bytes":58}"#;

#[test]
fn parses_proc_stat_and_counts_cpus() {
    let text = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\nintr 1\n";
    let t = parse_proc_stat(text).unwrap();
    assert_eq!((t.busy, t.total, t.logical_cpus), (150, 1000, 2));
}

#[test]
fn cpu_bps_from_two_readings() {
    let prev = CpuTimes { busy: 150, total: 1000, logical_cpus: 2 };
    let cur = CpuTimes { busy: 400, total: 1500, logical_cpus: 2 };
    assert_eq!(cpu_busy_bps(&prev, &cur), 5000);
    assert_eq!(cpu_busy_bps(&prev, &prev), 0);
}

#[test]
fn parses_meminfo() {
    let text = "MemTotal:       16384 kB\nMemFree: 1 kB\nMemAvailable:    4096 kB\n";
    assert_eq!(parse_meminfo(text), Some((16777216, 4194304)));
    assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
}

fn host_file(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let p = dir.path().join("host-load.json");
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn fresh_host_file_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let p = host_file(&dir, HOST_FILE);
    match read_host_load(&p, 1500) {
        HostLoad::Fresh(h) => assert_eq!(h.cpu_busy_bps, 6100),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stale_host_file_falls_back_to_linux_and_says_stale() {
    let dir = tempfile::tempdir().unwrap();
    let p = host_file(&dir, HOST_FILE);
    assert_eq!(read_host_load(&p, 1000 + HOST_FRESH_MS + 1), HostLoad::Stale);
    let config = MachineConfig { host_load_file: Some(p), ..MachineConfig::default() };
    let mut sampler = Sampler::new(config, dir.path().to_path_buf());
    let report = sampler.sample(1000 + HOST_FRESH_MS + 1).unwrap();
    assert_eq!(report.source, LoadSource::Linux);
    assert_eq!(report.host_probe, HostProbe::Stale);
}

#[test]
fn missing_or_wrong_schema_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_host_load(&dir.path().join("nope.json"), 1), HostLoad::Missing);
    let p = host_file(&dir, &HOST_FILE.replace("aq-host-load-1", "other"));
    assert_eq!(read_host_load(&p, 1000), HostLoad::Missing);
    let p = host_file(&dir, "not json");
    assert_eq!(read_host_load(&p, 1000), HostLoad::Missing);
}

#[test]
fn machine_config_defaults_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let missing = MachineConfig::load_optional(&dir.path().join("machine.json")).unwrap();
    assert_eq!(missing.interval_secs, 15);
    assert_eq!(missing.kind, crate::MachineKind::Local);
    let p = dir.path().join("machine.json");
    std::fs::write(&p, r#"{"interval_secs":4}"#).unwrap();
    assert!(MachineConfig::load_optional(&p).is_err());
    std::fs::write(&p, r#"{"key":"desktop","host_load_file":null,"interval_secs":30}"#).unwrap();
    assert_eq!(MachineConfig::load_optional(&p).unwrap().key.as_deref(), Some("desktop"));
}

#[test]
fn pinned_fixtures_round_trip() {
    for text in [
        include_str!("../../../protocol/pinned/worker-machine/v1/fixtures/report-windows-host.json"),
        include_str!("../../../protocol/pinned/worker-machine/v1/fixtures/report-linux.json"),
    ] {
        let report: MachineReport = serde_json::from_str(text).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }
}

#[test]
fn pinned_contract_digests_match() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol/pinned/worker-machine");
    let source: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("source.json")).unwrap()).unwrap();
    let digests = source["sha256"].as_object().unwrap();
    assert_eq!(digests.len(), 5);
    for (file, want) in digests {
        let got = format!("{:x}", Sha256::digest(std::fs::read(root.join(file)).unwrap()));
        assert_eq!(want.as_str().unwrap(), got, "{file}");
    }
}

/// Reads the file the Windows probe wrote. Run right after
/// `aq-host-load.ps1 -Once`: `cargo test -p aq-machine-load -- --ignored real_host_file`.
#[test]
#[ignore = "needs the Windows probe to have just written the file"]
fn real_host_file() {
    let path = std::path::Path::new("/mnt/c/ProgramData/agent-q/host-load.json");
    let now = crate::now_ms();
    assert!(matches!(read_host_load(path, now), HostLoad::Fresh(_)));
}
