//! Validate `hostRequirements` from devcontainer.json against the host system.

use std::path::Path;

use serde_json::Value;
use tracing::debug;

/// A single host requirement check result.
#[derive(Debug)]
pub struct RequirementCheck {
    pub name: String,
    pub required: String,
    pub actual: String,
    pub met: bool,
}

/// Result of validating all host requirements.
#[derive(Debug)]
pub struct ValidationResult {
    pub checks: Vec<RequirementCheck>,
    pub all_met: bool,
}

/// Validate host against `hostRequirements` from a devcontainer config.
///
/// Returns checks for each requirement found. If `hostRequirements` is absent,
/// returns an empty result with `all_met = true`.
pub fn validate(config: &Value, workspace_root: &Path) -> ValidationResult {
    let mut checks = Vec::new();

    let Some(reqs) = config.get("hostRequirements").and_then(Value::as_object) else {
        return ValidationResult {
            checks,
            all_met: true,
        };
    };

    if let Some(cpus) = reqs.get("cpus").and_then(Value::as_u64) {
        check_cpus(&mut checks, cpus);
    }
    if let Some(mem_str) = reqs.get("memory").and_then(Value::as_str)
        && let Some(required_bytes) = parse_memory_string(mem_str)
    {
        check_memory(&mut checks, required_bytes);
    }
    if let Some(storage_str) = reqs.get("storage").and_then(Value::as_str)
        && let Some(required_bytes) = parse_memory_string(storage_str)
    {
        check_storage(&mut checks, required_bytes, workspace_root);
    }
    if let Some(gpu) = reqs.get("gpu") {
        check_gpu(&mut checks, gpu);
    }

    let all_met = checks.iter().all(|c| c.met);
    ValidationResult { checks, all_met }
}

fn check_cpus(checks: &mut Vec<RequirementCheck>, cpus: u64) {
    let available = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .map_or(1u64, |v| v as u64);
    checks.push(RequirementCheck {
        name: "cpus".to_string(),
        required: cpus.to_string(),
        actual: available.to_string(),
        met: available >= cpus,
    });
}

fn check_memory(checks: &mut Vec<RequirementCheck>, required_bytes: u64) {
    let actual_bytes = get_total_memory();
    let met = actual_bytes.is_some_and(|a| a >= required_bytes);
    checks.push(RequirementCheck {
        name: "memory".to_string(),
        required: format_bytes(required_bytes),
        actual: actual_bytes.map_or_else(|| "unknown".to_string(), format_bytes),
        met,
    });
}

fn check_storage(checks: &mut Vec<RequirementCheck>, required_bytes: u64, workspace_root: &Path) {
    let actual_bytes = get_available_storage(workspace_root);
    let met = actual_bytes.is_some_and(|a| a >= required_bytes);
    checks.push(RequirementCheck {
        name: "storage".to_string(),
        required: format_bytes(required_bytes),
        actual: actual_bytes.map_or_else(|| "unknown".to_string(), format_bytes),
        met,
    });
}

fn check_gpu(checks: &mut Vec<RequirementCheck>, gpu: &Value) {
    let (required, optional) = match gpu {
        Value::Bool(true) | Value::Object(_) => (true, false),
        Value::String(s) if s == "optional" => (true, true),
        _ => (false, false),
    };

    if required {
        let gpu_available = check_gpu_available();
        checks.push(RequirementCheck {
            name: if optional {
                "gpu (optional)".to_string()
            } else {
                "gpu".to_string()
            },
            required: "available".to_string(),
            actual: if gpu_available {
                "available".to_string()
            } else {
                "not detected".to_string()
            },
            met: gpu_available || optional,
        });
    }
}

/// Parse a memory/storage string like "8gb", "512mb", "4096" into bytes.
pub fn parse_memory_string(s: &str) -> Option<u64> {
    crate::config_map::run_args::parse_byte_size(s).and_then(|v| u64::try_from(v).ok())
}

/// Get total system memory in bytes.
fn get_total_memory() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let rest = rest.trim();
                    if let Some(kb_str) =
                        rest.strip_suffix("kB").or_else(|| rest.strip_suffix("KB"))
                    {
                        return kb_str.trim().parse::<u64>().ok().map(|kb| kb * 1024);
                    }
                }
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Get available storage on the filesystem containing `path`.
fn get_available_storage(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("--output=avail")
        .arg("-B1")
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        let output = std::process::Command::new("df")
            .arg("-k")
            .arg(path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8(output.stdout).ok()?;
        let line = stdout.lines().nth(1)?;
        let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        return Some(avail_kb * 1024);
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.lines().nth(1)?.trim().parse::<u64>().ok()
}

/// Check if a GPU is available on the system.
fn check_gpu_available() -> bool {
    if Path::new("/dev/nvidia0").exists() {
        return true;
    }
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .arg("--query-gpu=name")
        .arg("--format=csv,noheader")
        .output()
        && output.status.success()
        && !output.stdout.is_empty()
    {
        return true;
    }
    debug!("No GPU detected (checked /dev/nvidia0 and nvidia-smi)");
    false
}

/// Format bytes into a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;

    if bytes >= GB {
        let whole = bytes / GB;
        let frac = (bytes % GB) * 10 / GB;
        format!("{whole}.{frac} GB")
    } else if bytes >= MB {
        let whole = bytes / MB;
        let frac = (bytes % MB) * 10 / MB;
        format!("{whole}.{frac} MB")
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parse_memory_string ──────────────────────────────────────────────

    #[test]
    fn parse_memory_gb() {
        assert_eq!(parse_memory_string("8gb"), Some(8 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_mb() {
        assert_eq!(parse_memory_string("512mb"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_raw_bytes() {
        assert_eq!(parse_memory_string("1048576"), Some(1_048_576));
    }

    #[test]
    fn parse_memory_case_insensitive() {
        assert_eq!(parse_memory_string("2GB"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_empty() {
        assert_eq!(parse_memory_string(""), None);
    }

    #[test]
    fn parse_memory_kb() {
        assert_eq!(parse_memory_string("1024kb"), Some(1024 * 1024));
    }

    #[test]
    fn parse_memory_tb() {
        assert_eq!(parse_memory_string("1tb"), Some(1024 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_short_suffix_g() {
        assert_eq!(parse_memory_string("4g"), Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_short_suffix_m() {
        assert_eq!(parse_memory_string("256m"), Some(256 * 1024 * 1024));
    }

    #[test]
    fn parse_memory_invalid_string() {
        assert_eq!(parse_memory_string("notanumber"), None);
    }

    #[test]
    fn parse_memory_whitespace_trimmed() {
        assert_eq!(parse_memory_string("  4gb  "), Some(4 * 1024 * 1024 * 1024));
    }

    // ── format_bytes ─────────────────────────────────────────────────────

    #[test]
    fn format_bytes_gb() {
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(512 * 1024 * 1024), "512.0 MB");
    }

    #[test]
    fn format_bytes_small() {
        assert_eq!(format_bytes(500), "500 bytes");
    }

    #[test]
    fn format_bytes_boundary() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 bytes");
    }

    #[test]
    fn format_bytes_fractional_gb() {
        // 1.5 GB = 1 GB + 512 MB
        let bytes = 1024 * 1024 * 1024 + 512 * 1024 * 1024;
        assert_eq!(format_bytes(bytes), "1.5 GB");
    }

    #[test]
    fn format_bytes_fractional_mb() {
        // 1.5 MB = 1 MB + 512 KB
        let bytes = 1024 * 1024 + 512 * 1024;
        assert_eq!(format_bytes(bytes), "1.5 MB");
    }

    #[test]
    fn format_bytes_just_under_mb() {
        assert_eq!(format_bytes(1024 * 1024 - 1), "1048575 bytes");
    }

    // ── validate ─────────────────────────────────────────────────────────

    #[test]
    fn validate_no_requirements() {
        let config = json!({"image": "ubuntu"});
        let result = validate(&config, Path::new("/tmp"));
        assert!(result.all_met);
        assert!(result.checks.is_empty());
    }

    #[test]
    fn validate_cpu_requirement_met() {
        let config = json!({"hostRequirements": {"cpus": 1}});
        let result = validate(&config, Path::new("/tmp"));
        assert!(!result.checks.is_empty());
        assert!(result.checks[0].met);
    }

    #[test]
    fn validate_cpu_requirement_extreme() {
        let config = json!({"hostRequirements": {"cpus": 99999}});
        let result = validate(&config, Path::new("/tmp"));
        assert!(!result.checks.is_empty());
        assert!(!result.checks[0].met);
    }

    #[test]
    fn validate_gpu_optional_always_met() {
        let config = json!({"hostRequirements": {"gpu": "optional"}});
        let result = validate(&config, Path::new("/tmp"));
        assert!(!result.checks.is_empty());
        assert!(result.checks[0].met);
    }

    #[test]
    fn validate_memory_requirement() {
        // Request 1 byte of memory -- any real system should have this
        let config = json!({"hostRequirements": {"memory": "1b"}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].name, "memory");
        assert!(result.checks[0].met);
    }

    #[test]
    fn validate_memory_extreme_not_met() {
        // Request 1 PB of memory -- no system has this
        let config = json!({"hostRequirements": {"memory": "999999tb"}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert!(!result.checks[0].met);
    }

    #[test]
    fn validate_storage_requirement() {
        // Request 1 byte of storage at /tmp -- always available
        let config = json!({"hostRequirements": {"storage": "1b"}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].name, "storage");
        assert!(result.checks[0].met);
    }

    #[test]
    fn validate_storage_extreme_not_met() {
        let config = json!({"hostRequirements": {"storage": "999999tb"}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert!(!result.checks[0].met);
    }

    #[test]
    fn validate_gpu_bool_true_checked() {
        let config = json!({"hostRequirements": {"gpu": true}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].name, "gpu");
        // CI/test hosts typically don't have GPUs
    }

    #[test]
    fn validate_gpu_bool_false_skipped() {
        let config = json!({"hostRequirements": {"gpu": false}});
        let result = validate(&config, Path::new("/tmp"));
        assert!(result.checks.is_empty());
    }

    #[test]
    fn validate_gpu_object_treated_as_required() {
        let config = json!({"hostRequirements": {"gpu": {"cores": 4}}});
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].name, "gpu");
    }

    #[test]
    fn validate_multiple_requirements() {
        let config = json!({
            "hostRequirements": {
                "cpus": 1,
                "memory": "1b",
                "storage": "1b",
                "gpu": "optional"
            }
        });
        let result = validate(&config, Path::new("/tmp"));
        assert_eq!(result.checks.len(), 4);
    }

    #[test]
    fn validate_all_met_reflects_individual_checks() {
        let config = json!({
            "hostRequirements": {
                "cpus": 1,
                "memory": "999999tb"
            }
        });
        let result = validate(&config, Path::new("/tmp"));
        assert!(!result.all_met);
    }

    #[test]
    fn validate_host_requirements_null_treated_as_absent() {
        let config = json!({"hostRequirements": null});
        let result = validate(&config, Path::new("/tmp"));
        assert!(result.all_met);
        assert!(result.checks.is_empty());
    }

    #[test]
    fn validate_invalid_memory_string_skipped() {
        let config = json!({"hostRequirements": {"memory": "not-a-size"}});
        let result = validate(&config, Path::new("/tmp"));
        // Invalid memory string is silently skipped (no check added)
        assert!(result.checks.is_empty());
    }

    // ── check_gpu helper ─────────────────────────────────────────────────

    #[test]
    fn check_gpu_optional_label() {
        let mut checks = Vec::new();
        check_gpu(&mut checks, &json!("optional"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "gpu (optional)");
        assert!(checks[0].met); // optional always met
    }

    #[test]
    fn check_gpu_required_label() {
        let mut checks = Vec::new();
        check_gpu(&mut checks, &json!(true));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "gpu");
    }

    #[test]
    fn check_gpu_false_no_check() {
        let mut checks = Vec::new();
        check_gpu(&mut checks, &json!(false));
        assert!(checks.is_empty());
    }

    #[test]
    fn check_gpu_random_string_no_check() {
        let mut checks = Vec::new();
        check_gpu(&mut checks, &json!("foobar"));
        assert!(checks.is_empty());
    }

    // ── check_memory / check_storage helpers ─────────────────────────────

    #[test]
    fn check_memory_creates_check() {
        let mut checks = Vec::new();
        check_memory(&mut checks, 1);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "memory");
        assert!(checks[0].met);
    }

    #[test]
    fn check_storage_creates_check() {
        let mut checks = Vec::new();
        check_storage(&mut checks, 1, Path::new("/tmp"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "storage");
        assert!(checks[0].met);
    }

    #[test]
    fn check_cpus_creates_check() {
        let mut checks = Vec::new();
        check_cpus(&mut checks, 1);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "cpus");
        assert!(checks[0].met);
    }
}
