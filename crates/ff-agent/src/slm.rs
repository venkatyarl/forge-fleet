//! Thin, synchronous client for a local GGUF small language model.
//!
//! The wrapper deliberately uses `llama-cli` instead of linking its C++ ABI:
//! model/runtime upgrades stay independent from `ff-agent`, while a crashed or
//! timed-out inference process can be isolated and reaped safely.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::System;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const RAM_HEADROOM_PERCENT: u64 = 25;

/// Run a single prediction against the GGUF model in `FORGEFLEET_SLM_MODEL`.
///
/// `FORGEFLEET_SLM_BIN` may override `llama-cli`, and
/// `FORGEFLEET_SLM_TIMEOUT_SECS` may override the 120-second deadline (capped
/// at 600 seconds). Failures are returned as deterministic `SLM error: ...`
/// strings because this intentionally minimal API has no separate error type.
pub fn predict(text: &str) -> String {
    match predict_inner(text) {
        Ok(output) => output,
        Err(error) => format!("SLM error: {error}"),
    }
}

fn predict_inner(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("prediction text is empty".into());
    }

    let model = std::env::var_os("FORGEFLEET_SLM_MODEL")
        .map(PathBuf::from)
        .ok_or_else(|| "FORGEFLEET_SLM_MODEL is not set".to_string())?;
    validate_model(&model)?;

    let available_ram = available_ram_bytes();
    let model_bytes = model
        .metadata()
        .map_err(|error| format!("cannot stat model '{}': {error}", model.display()))?
        .len();
    let required_ram =
        model_bytes.saturating_add(model_bytes.saturating_mul(RAM_HEADROOM_PERCENT) / 100);
    if available_ram > 0 && required_ram > available_ram {
        return Err(format!(
            "model needs about {} MiB including runtime headroom, but only {} MiB RAM is available",
            required_ram / (1024 * 1024),
            available_ram / (1024 * 1024)
        ));
    }

    let binary = std::env::var_os("FORGEFLEET_SLM_BIN").unwrap_or_else(|| "llama-cli".into());
    let timeout = configured_timeout();
    let mut command = Command::new(&binary);
    command
        .arg("-m")
        .arg(&model)
        .arg("-p")
        .arg(text)
        .args(["-n", "256", "--temp", "0", "--no-display-prompt"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "failed to start '{}': {error}",
            Path::new(&binary).display()
        )
    })?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("prediction timed out after {}s", timeout.as_secs()));
            }
            Err(error) => return Err(format!("failed while waiting for llama-cli: {error}")),
        }
    };

    let stdout = join_output(stdout_reader, "stdout")?;
    let stderr = join_output(stderr_reader, "stderr")?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(format!(
            "llama-cli exited with {}: {}",
            status,
            truncate(detail.trim(), 2_000)
        ));
    }

    let output =
        String::from_utf8(stdout).map_err(|_| "llama-cli returned non-UTF-8 output".to_string())?;
    let output = output.trim();
    if output.is_empty() {
        return Err("llama-cli returned empty output".into());
    }
    Ok(output.to_string())
}

fn validate_model(path: &Path) -> Result<(), String> {
    if path.extension().and_then(|value| value.to_str()) != Some("gguf") {
        return Err(format!("model '{}' is not a .gguf file", path.display()));
    }
    let mut magic = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|error| format!("cannot read model '{}': {error}", path.display()))?;
    if &magic != b"GGUF" {
        return Err(format!(
            "model '{}' has an invalid GGUF header",
            path.display()
        ));
    }
    if let Some(quant) = quantization_from_name(path) {
        if !supported_quantization(&quant) {
            return Err(format!(
                "model quantization {quant} is larger than supported Q4_K_M"
            ));
        }
    }
    Ok(())
}

fn quantization_from_name(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_str()?.to_ascii_uppercase();
    name.split(|ch: char| ch == '-' || ch == '.')
        .find(|part| {
            part.strip_prefix('Q')
                .or_else(|| part.strip_prefix("IQ"))
                .or_else(|| part.strip_prefix('F'))
                .is_some_and(|suffix| suffix.starts_with(char::is_numeric))
        })
        .map(str::to_string)
}

fn supported_quantization(quant: &str) -> bool {
    let digits = quant
        .trim_start_matches('I')
        .trim_start_matches('Q')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    match digits.parse::<u8>() {
        Ok(bits @ 0..=3) => bits <= 3,
        Ok(4) => !quant.ends_with("_L"),
        _ => false,
    }
}

fn available_ram_bytes() -> u64 {
    let mut system = System::new();
    system.refresh_memory();
    system.available_memory()
}

fn configured_timeout() -> Duration {
    std::env::var("FORGEFLEET_SLM_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
        .min(MAX_TIMEOUT)
}

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_output(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| format!("llama-cli {stream} reader panicked"))?
        .map_err(|error| format!("failed reading llama-cli {stream}: {error}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: the child was placed in a new process group whose id is its pid.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn accepts_q4_k_m_and_smaller_quantizations() {
        for quant in ["Q2_K", "Q3_K_M", "Q4_0", "Q4_K_S", "Q4_K_M", "IQ4_XS"] {
            assert!(supported_quantization(quant), "{quant}");
        }
    }

    #[test]
    fn rejects_larger_quantizations() {
        for quant in ["Q4_K_L", "Q5_K_M", "Q6_K", "Q8_0", "F16"] {
            assert!(!supported_quantization(quant), "{quant}");
        }
    }

    #[test]
    fn extracts_quantization_without_mistaking_model_name() {
        let path = Path::new("Qwen2.5-Coder-7B-Q4_K_M.gguf");
        assert_eq!(quantization_from_name(path).as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn model_validation_checks_gguf_magic() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("tiny-Q4_K_M.gguf");
        File::create(&model).unwrap().write_all(b"nope").unwrap();
        assert!(validate_model(&model).unwrap_err().contains("invalid GGUF"));
    }
}
