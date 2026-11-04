//! Controls the Cursor desktop process before and after an explicit takeover.

use std::time::Duration;

use tokio::{
    process::Command,
    time::{sleep, Instant},
};

use crate::{Error, Result};

#[cfg(target_os = "linux")]
use std::process::Stdio;

#[cfg(windows)]
use std::path::PathBuf;

const CURSOR_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

pub async fn terminate_cursor() -> Result<()> {
    terminate_platform_cursor().await
}

/// Restarts Cursor with `--test-backend-url` so its Agent transport uses the
/// local service. Cursor's extension host does not honor `http.proxy`, and with
/// HTTP/2 disabled it selects `cursorCreds.backendUrl` for Run/Bidi. The launch
/// flag is Cursor's supported override; it keeps the official account and
/// authentication flow intact.
/// Stops Cursor and waits until its process is gone. Callers that need to
/// patch Cursor's persisted state must use this so Cursor cannot overwrite the
/// write from its in-memory applicationUser snapshot.
pub async fn stop_cursor() -> Result<()> {
    terminate_cursor().await?;
    wait_for_cursor_exit(CURSOR_EXIT_TIMEOUT).await
}

pub async fn launch_cursor_with_backend(backend_url: &str) -> Result<()> {
    launch_platform_cursor(backend_url).await
}

pub async fn cursor_running() -> Result<bool> {
    cursor_running_platform().await
}

/// Returns the `--test-backend-url` value of the running Cursor process, if
/// any. Used at startup to refresh a stale local URL after a service-port
/// change without touching Cursor launched for other backends.
pub async fn cursor_backend_override() -> Result<Option<String>> {
    backend_override_platform().await
}

/// Returns whether the running Cursor process carries the expected
/// `MOLTEN_GLASS_BACKEND_URL`. `None` means the platform cannot inspect
/// another process's environment (Windows), so the caller must not force a
/// restart based on this value.
pub async fn cursor_has_molten_backend(expected: &str) -> Result<Option<bool>> {
    molten_backend_platform(expected).await
}

#[cfg(target_os = "macos")]
async fn molten_backend_platform(expected: &str) -> Result<Option<bool>> {
    let Some(pid) = cursor_pid_platform().await? else {
        return Ok(Some(false));
    };
    let output = Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
        .await?;
    if !output.status.success() {
        return Err(Error::Config(
            "failed to inspect the Cursor environment".into(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(Some(
        parse_env_value(&text, "MOLTEN_GLASS_BACKEND_URL").as_deref() == Some(expected),
    ))
}

#[cfg(target_os = "linux")]
async fn molten_backend_platform(expected: &str) -> Result<Option<bool>> {
    let Some(pid) = cursor_pid_platform().await? else {
        return Ok(Some(false));
    };
    let data = tokio::fs::read(format!("/proc/{pid}/environ")).await?;
    let text = String::from_utf8_lossy(&data).replace('\0', " ");
    Ok(Some(
        parse_env_value(&text, "MOLTEN_GLASS_BACKEND_URL").as_deref() == Some(expected),
    ))
}

#[cfg(target_os = "windows")]
async fn molten_backend_platform(_expected: &str) -> Result<Option<bool>> {
    Ok(None)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn molten_backend_platform(_expected: &str) -> Result<Option<bool>> {
    Ok(None)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn cursor_pid_platform() -> Result<Option<u32>> {
    for name in ["Cursor", "cursor"] {
        let output = Command::new("pgrep").args(["-x", name]).output().await?;
        if output.status.success() {
            if let Some(pid) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .and_then(|line| line.trim().parse().ok())
            {
                return Ok(Some(pid));
            }
        }
    }
    Ok(None)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_env_value(output: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    output.split_whitespace().find_map(|part| {
        part.strip_prefix(&prefix)
            .map(|value| value.trim_matches(['"', '\'']).to_owned())
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn backend_override_platform() -> Result<Option<String>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .await?;
    if !output.status.success() {
        return Err(Error::Config(
            "failed to inspect the Cursor command line".into(),
        ));
    }
    Ok(parse_backend_override(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_backend_override(output: &str) -> Option<String> {
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(_pid) = parts.next() else {
            continue;
        };
        let Some(executable) = parts.next() else {
            continue;
        };
        if !matches!(executable, "Cursor" | "cursor")
            && !executable.ends_with("/Cursor")
            && !executable.ends_with("/cursor")
        {
            continue;
        }
        while let Some(part) = parts.next() {
            if part == "--test-backend-url" {
                return parts
                    .next()
                    .map(|value| value.trim_matches(['"', '\'']).to_owned());
            }
            if let Some(value) = part.strip_prefix("--test-backend-url=") {
                return Some(value.trim_matches(['"', '\'']).to_owned());
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
async fn backend_override_platform() -> Result<Option<String>> {
    // tasklist cannot expose the full command line. The startup reconciler
    // therefore treats Windows as "unknown" and leaves Cursor untouched.
    Ok(None)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn backend_override_platform() -> Result<Option<String>> {
    Err(Error::Config(format!(
        "inspecting the Cursor command line is unsupported on {}",
        std::env::consts::OS
    )))
}

async fn wait_for_cursor_exit(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !cursor_running_platform().await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Config(
                "Cursor did not exit before the backend restart".into(),
            ));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "macos")]
async fn cursor_running_platform() -> Result<bool> {
    process_running("Cursor").await
}

#[cfg(target_os = "linux")]
async fn cursor_running_platform() -> Result<bool> {
    Ok(process_running("cursor").await? || process_running("Cursor").await?)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn process_running(name: &str) -> Result<bool> {
    let status = Command::new("pgrep").args(["-x", name]).status().await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::Config(format!(
            "failed to inspect the {name} process"
        ))),
    }
}

#[cfg(target_os = "macos")]
async fn terminate_platform_cursor() -> Result<()> {
    terminate_unix_process("Cursor").await
}

#[cfg(target_os = "linux")]
async fn terminate_platform_cursor() -> Result<()> {
    terminate_unix_process("cursor").await?;
    terminate_unix_process("Cursor").await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn terminate_unix_process(name: &str) -> Result<()> {
    if !process_running(name).await? {
        return Ok(());
    }
    let terminated = Command::new("pkill").args(["-x", name]).status().await?;
    if terminated.success() || terminated.code() == Some(1) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "failed to terminate the {name} process"
        )))
    }
}

#[cfg(target_os = "macos")]
async fn launch_platform_cursor(backend_url: &str) -> Result<()> {
    // The glass Settings → Models page runs in a separate molten engine that
    // reads MOLTEN_GLASS_BACKEND_URL instead of --test-backend-url. Point both
    // at the local service so the settings catalog includes BYOK models.
    let molten_backend = format!("MOLTEN_GLASS_BACKEND_URL={backend_url}");
    let status = Command::new("open")
        .args([
            "--env",
            molten_backend.as_str(),
            "-a",
            "Cursor",
            "--args",
            "--test-backend-url",
            backend_url,
        ])
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "failed to launch Cursor with the local backend ({status})"
        )))
    }
}

#[cfg(target_os = "linux")]
async fn launch_platform_cursor(backend_url: &str) -> Result<()> {
    let mut child = Command::new("cursor")
        .env("MOLTEN_GLASS_BACKEND_URL", backend_url)
        .args(["--test-backend-url", backend_url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::Config(format!("failed to launch Cursor: {error}")))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cursor_running_platform() -> Result<bool> {
    let mut list = Command::new("tasklist");
    hide_console(&mut list);
    let processes = list
        .args(["/FI", "IMAGENAME eq Cursor.exe", "/NH", "/FO", "CSV"])
        .output()
        .await?;
    if !processes.status.success() {
        return Err(Error::Config(
            "failed to inspect the Cursor.exe process".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&processes.stdout)
        .to_ascii_lowercase()
        .contains("cursor.exe"))
}

#[cfg(target_os = "windows")]
async fn terminate_platform_cursor() -> Result<()> {
    if !cursor_running_platform().await? {
        return Ok(());
    }
    let mut kill = Command::new("taskkill");
    hide_console(&mut kill);
    let terminated = kill
        .args(["/F", "/T", "/IM", "Cursor.exe"])
        .status()
        .await?;
    if terminated.success() {
        Ok(())
    } else {
        Err(Error::Config(
            "failed to terminate the Cursor.exe process".into(),
        ))
    }
}

#[cfg(target_os = "windows")]
async fn launch_platform_cursor(backend_url: &str) -> Result<()> {
    if let Some(executable) = windows_cursor_executable().filter(|path| path.exists()) {
        let mut child = Command::new(executable)
            .env("MOLTEN_GLASS_BACKEND_URL", backend_url)
            .args(["--test-backend-url", backend_url])
            .spawn()
            .map_err(|error| Error::Config(format!("failed to launch Cursor: {error}")))?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        return Ok(());
    }
    let mut command = Command::new("cmd");
    hide_console(&mut command);
    let status = command
        .env("MOLTEN_GLASS_BACKEND_URL", backend_url)
        .args([
            "/C",
            "start",
            "",
            "Cursor.exe",
            "--test-backend-url",
            backend_url,
        ])
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "failed to launch Cursor with the local backend ({status})"
        )))
    }
}

#[cfg(target_os = "windows")]
fn windows_cursor_executable() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|local| local.join("Programs").join("cursor").join("Cursor.exe"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn cursor_running_platform() -> Result<bool> {
    Err(Error::Config(format!(
        "controlling Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn terminate_platform_cursor() -> Result<()> {
    Err(Error::Config(format!(
        "terminating Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn launch_platform_cursor(_backend_url: &str) -> Result<()> {
    Err(Error::Config(format!(
        "launching Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::{parse_backend_override, parse_env_value};

    #[test]
    fn parses_test_backend_url_from_ps_output() {
        let output = "\
  123 /Applications/Cursor.app/Contents/MacOS/Cursor --test-backend-url http://127.0.0.1:59112
  124 /Applications/Cursor.app/Contents/Frameworks/Cursor Helper --type=renderer
";
        assert_eq!(
            parse_backend_override(output).as_deref(),
            Some("http://127.0.0.1:59112")
        );
    }

    #[test]
    fn parses_equals_form_and_ignores_unmanaged_processes() {
        let output = "\
  42 /usr/bin/cursor --test-backend-url=http://127.0.0.1:6000
  43 /usr/bin/other-app --test-backend-url http://127.0.0.1:7000
";
        assert_eq!(
            parse_backend_override(output).as_deref(),
            Some("http://127.0.0.1:6000")
        );
        assert_eq!(parse_backend_override("  44 /usr/bin/Cursor\n"), None);
    }

    #[test]
    fn parses_molten_backend_from_process_environment() {
        let output = "PID COMMAND MOLTEN_GLASS_BACKEND_URL=http://127.0.0.1:59112 OTHER=1";
        assert_eq!(
            parse_env_value(output, "MOLTEN_GLASS_BACKEND_URL").as_deref(),
            Some("http://127.0.0.1:59112")
        );
        assert_eq!(
            parse_env_value("PID COMMAND OTHER=1", "MOLTEN_GLASS_BACKEND_URL"),
            None
        );
    }
}
