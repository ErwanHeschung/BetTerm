//! Shell-independent startup banner. betterm builds a fastfetch-style system-info
//! banner itself (in Rust) and injects it into the grid at launch, so it shows up
//! no matter which shell is configured (PowerShell, Git Bash, WSL, ...).

use sysinfo::System;

/// Build the banner as a string of ANSI/VT escapes + text, with `\r\n` line
/// endings ready to feed through the terminal parser.
pub fn build(shell: &str, gpu: &str) -> String {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();

    let violet = "\x1b[38;2;170;120;255m";
    let pink = "\x1b[38;2;255;105;180m";
    let dim = "\x1b[38;2;130;130;150m";
    let reset = "\x1b[0m";

    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let host = System::host_name().unwrap_or_default();
    let os = System::long_os_version().unwrap_or_else(|| "Windows".into());
    let kernel = System::kernel_version().unwrap_or_default();
    let up = fmt_uptime(System::uptime());
    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();
    let to_gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let total = to_gb(sys.total_memory());
    let used = to_gb(sys.used_memory());
    let (manuf, model) = host_model();
    let host_line = format!("{manuf} {model}").trim().to_string();
    let shell_name = shell_basename(shell);

    let logo = [
        r"  ___      _   _              ",
        r" | _ ) ___| |_| |_ ___ _ _ _ __ ",
        r" | _ \/ -_)  _|  _/ -_) '_| '  \",
        r" |___/\___|\__|\__\___|_| |_|_|_|",
    ];
    let info = [
        format!("{pink}{user}{reset}@{pink}{host}{reset}"),
        format!("{dim}-----------------{reset}"),
        format!("{violet} OS     {reset} {os}"),
        format!("{violet} Host   {reset} {host_line}"),
        format!("{violet} Kernel {reset} {kernel}"),
        format!("{violet} Uptime {reset} {up}"),
        format!("{violet} Shell  {reset} {shell_name}"),
        format!("{violet} CPU    {reset} {cpu}"),
        format!("{violet} GPU    {reset} {gpu}"),
        format!("{violet} Memory {reset} {used:.1} GB / {total:.1} GB"),
        format!("{violet} Term   {reset} betterm"),
    ];

    let maxlen = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 3;
    let rows = logo.len().max(info.len());
    let mut out = String::from("\r\n");
    for i in 0..rows {
        let l = logo.get(i).copied().unwrap_or("");
        let pad = " ".repeat(maxlen.saturating_sub(l.chars().count()));
        let r = info.get(i).cloned().unwrap_or_default();
        out.push_str(&format!("{violet}{l}{pad}{reset}{r}\r\n"));
    }
    out.push_str("\r\n");
    out
}

fn fmt_uptime(secs: u64) -> String {
    format!(
        "{}d {}h {}m",
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60
    )
}

fn shell_basename(shell: &str) -> String {
    std::path::Path::new(shell)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(shell)
        .to_string()
}

/// Read system manufacturer + product name from the registry (the "Host" line).
fn host_model() -> (String, String) {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    match hklm.open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS") {
        Ok(k) => (
            k.get_value("SystemManufacturer").unwrap_or_default(),
            k.get_value("SystemProductName").unwrap_or_default(),
        ),
        Err(_) => (String::new(), String::new()),
    }
}
