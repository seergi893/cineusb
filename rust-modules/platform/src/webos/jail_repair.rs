//! User-confirmed repair for the k5lp/k3lp Developer Mode jail.
//!
//! No repair runs at boot. PlayerAdapter calls `execute` only after Player.repair accepts
//! explicit confirmation. That app-owned attempt survives every screen and session reset,
//! including timeout: the remote shell may still run after LS2 gives up.

use crate::tv::sandbox::Failure;
use serde_json::Value;
use std::path::Path;
#[cfg(all(not(feature = "hostsim"), not(any(test, feature = "test-support"))))]
use std::time::Duration;

#[cfg(all(not(feature = "hostsim"), not(any(test, feature = "test-support"))))]
const EXEC_URI: &str = "luna://org.webosbrew.hbchannel.service/exec";
#[cfg(all(not(feature = "hostsim"), not(any(test, feature = "test-support"))))]
const BUDGET: Duration = Duration::from_secs(10);
const OK_MARKER: &str = "PLXNATIVE_JAIL_REPAIR_OK_74";
const NOT_ROOT_MARKER: &str = "PLXNATIVE_JAIL_REPAIR_NOT_ROOT_74";
const HBC_ABSENT_TEXT: &str = "Service does not exist: org.webosbrew.hbchannel.service.";

/// Called only by the PlayerAdapter worker after the owner accepts explicit confirmation.
pub fn execute() -> Result<(), Failure> {
    repair(
        call_hbc,
        plx_base::paths::app_dir(),
        plx_base::paths::app_id(),
        || device_readable(RTKMEM),
    )
}

const RTKMEM: &str = "/dev/rtkmem";

#[cfg(all(not(feature = "hostsim"), not(any(test, feature = "test-support"))))]
fn call_hbc(payload: &str) -> Result<String, Failure> {
    let registration = super::ls2::register().map_err(|_| Failure::HbcUnavailable)?;
    let budget = if payload.contains("--cineusb-delete-folder ") { Duration::from_secs(310) } else { BUDGET };
    registration
        .call(EXEC_URI, payload, budget)
        .map_err(|failure| match failure {
            super::ls2::Fail::Timeout => Failure::Timeout,
            super::ls2::Fail::Setup { .. } => Failure::HbcUnavailable,
        })
}

#[cfg(any(feature = "hostsim", test, feature = "test-support"))]
fn call_hbc(_payload: &str) -> Result<String, Failure> {
    Err(Failure::HbcUnavailable)
}

/// Ask the root service for one read-only file descriptor. The socket remains private to this
/// request, and the open file crosses the jail boundary without copying bytes or changing mounts.
#[cfg(target_os = "linux")]
pub fn open_usb_file(path: &Path, mount_id: Option<u64>) -> Result<std::fs::File, Failure> {
    use std::os::fd::AsRawFd;
    use std::os::unix::{ffi::OsStrExt, net::UnixListener};

    let path_hex = usb_path_hex(path)?;
    let mut template = *b"/tmp/cineusb-fd-XXXXXX\0";
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(Failure::StartFailed);
    }
    let directory = Path::new(std::ffi::OsStr::from_bytes(&template[..template.len() - 1]));
    let socket = directory.join("file.sock");
    let result = (|| {
        let listener = UnixListener::bind(&socket).map_err(|_| Failure::StartFailed)?;
        listener.set_nonblocking(true).map_err(|_| Failure::StartFailed)?;
        let slot = std::str::from_utf8(&template[template.len() - 7..template.len() - 1])
            .map_err(|_| Failure::StartFailed)?;
        if !slot.bytes().all(|byte| byte.is_ascii_alphanumeric()) { return Err(Failure::StartFailed); }
        let command = format!("--cineusb-open {} '{slot}' {} '{path_hex}'", std::process::id(), mount_id.unwrap_or(0));
        if run_usb_helper(&command)? != "CINEUSB_FD_OK" {
            plx_base::eventlog::log("cineusb: Homebrew USB open returned no descriptor confirmation");
            return Err(Failure::CommandFailed);
        }
        let (stream, _) = listener.accept().map_err(|_| Failure::Unreadable)?;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(), &mut len) };
        if rc != 0 || cred.uid != 0 { return Err(Failure::NotRoot); }
        stream.set_nonblocking(true).map_err(|_| Failure::Unreadable)?;
        let file = super::usb_fd::receive(&stream).map_err(|_| Failure::Unreadable)?;
        if !file.metadata().is_ok_and(|meta| meta.is_file()) { return Err(Failure::Unreadable); }
        Ok(file)
    })();
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_dir(directory);
    result
}

#[cfg(target_os = "linux")]
fn run_usb_helper(args: &str) -> Result<String, Failure> {
    let id = plx_base::paths::app_id();
    let dir = validated_install(plx_base::paths::app_dir(), id).ok_or(Failure::Unsupported)?;
    let prefix = dir.strip_suffix(&format!("/applications/{id}")).ok_or(Failure::Unsupported)?;
    let helper = format!("{prefix}/services/{id}.storage/plxnative-storage");
    let command = format!("exec '{helper}' {args}");
    let reply = call_hbc(&serde_json::json!({ "command": command }).to_string())?;
    let operation = args.split_ascii_whitespace().next().unwrap_or_default();
    let reply: Value = serde_json::from_str(&reply).map_err(|_| {
        plx_base::eventlog::log(&format!("cineusb: Homebrew {operation} returned invalid JSON"));
        Failure::CommandFailed
    })?;
    if reply.get("returnValue").and_then(Value::as_bool) != Some(true) {
        let detail = reply.get("stderrString").and_then(Value::as_str)
            .and_then(|stderr| stderr.lines().find(|line| line.starts_with("CineUSB USB request failed: ")));
        let code = reply.get("errorCode").and_then(Value::as_i64);
        plx_base::eventlog::log(&format!("cineusb: Homebrew {operation} failed code={code:?} {}",
            detail.unwrap_or("helper failure detail unavailable")));
        return Err(Failure::CommandFailed);
    }
    reply.get("stdoutString").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| {
        plx_base::eventlog::log(&format!("cineusb: Homebrew {operation} returned no stdout"));
        Failure::CommandFailed
    })
}

#[cfg(target_os = "linux")]
fn usb_path_hex(path: &Path) -> Result<String, Failure> {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt;
    if !(path.starts_with("/tmp/usb") || path.starts_with("/tmp/cineusb-volumes")) {
        return Err(Failure::Unsupported);
    }
    let mut hex = String::new();
    for byte in path.as_os_str().as_bytes() { let _ = write!(hex, "{byte:02x}"); }
    Ok(hex)
}

#[cfg(target_os = "linux")]
fn usb_unhex(encoded: &str) -> Result<std::ffi::OsString, Failure> {
    use std::os::unix::ffi::OsStringExt;
    if encoded.len() % 2 != 0 || encoded.len() > 8192 { return Err(Failure::CommandFailed); }
    let bytes = encoded.as_bytes().chunks_exact(2).map(|pair| {
        let digits = std::str::from_utf8(pair).map_err(|_| Failure::CommandFailed)?;
        u8::from_str_radix(digits, 16).map_err(|_| Failure::CommandFailed)
    }).collect::<Result<Vec<_>, _>>()?;
    Ok(std::ffi::OsString::from_vec(bytes))
}

#[cfg(target_os = "linux")]
pub type UsbVolume = (u64, std::path::PathBuf, Option<std::ffi::OsString>);

#[cfg(target_os = "linux")]
pub fn list_usb_volumes() -> Result<Vec<UsbVolume>, Failure> {
    let reply = run_usb_helper("--cineusb-volumes")?;
    let volumes: Vec<(u64, String, Option<String>)> = serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)?;
    volumes.into_iter().map(|(id, encoded, label)| {
        let path = std::path::PathBuf::from(usb_unhex(&encoded)?);
        if id == 0 || path == Path::new("/tmp/usb") || !path.starts_with("/tmp/usb") {
            return Err(Failure::CommandFailed);
        }
        Ok((id, path, label.as_deref().map(usb_unhex).transpose()?))
    }).collect()
}

#[cfg(target_os = "linux")]
pub type UsbEntry = (std::ffi::OsString, bool, Option<i64>, Option<u64>);

#[cfg(target_os = "linux")]
pub type UsbVolumeDetails = (Option<(u64, u64, u64, bool)>, Option<String>);

#[cfg(target_os = "linux")]
pub fn usb_volume_details(path: &Path, mount_id: u64) -> Result<UsbVolumeDetails, Failure> {
    let hex = usb_path_hex(path)?;
    let reply = run_usb_helper(&format!("--cineusb-volume-details {mount_id} '{hex}'"))?;
    serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)
}

#[cfg(target_os = "linux")]
pub fn list_usb_folder(path: &Path, mount_id: u64) -> Result<Vec<UsbEntry>, Failure> {
    let hex = usb_path_hex(path)?;
    let reply = run_usb_helper(&format!("--cineusb-list {mount_id} '{hex}'"))?;
    let entries: Vec<(String, bool, Option<i64>, Option<u64>)> = serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)?;
    entries.into_iter().map(|(encoded, dir, modified, size)| {
        let name = usb_unhex(&encoded)?;
        if Path::new(&name).file_name() != Some(name.as_os_str()) { return Err(Failure::CommandFailed); }
        Ok((name, dir, modified, size))
    }).collect()
}

#[cfg(target_os = "linux")]
pub fn delete_usb_video(path: &Path, mount_id: u64) -> Result<(), Failure> {
    usb_mutation("--cineusb-delete", path, mount_id)
}

#[cfg(target_os = "linux")]
pub fn eject_usb_disk(path: &Path, mount_id: u64) -> Result<(), String> {
    usb_report("--cineusb-eject", path, mount_id)
}

#[cfg(target_os = "linux")]
pub fn delete_usb_folder(path: &Path, mount_id: u64) -> Result<(), String> {
    usb_report("--cineusb-delete-folder", path, mount_id)
}

#[cfg(target_os = "linux")]
fn usb_report(operation: &str, path: &Path, mount_id: u64) -> Result<(), String> {
    let hex = usb_path_hex(path).map_err(|error| format!("{error:?}"))?;
    let reply = run_usb_helper(&format!("{operation} {mount_id} '{hex}'")).map_err(|error| format!("{error:?}"))?;
    match serde_json::from_str::<Option<String>>(&reply).map_err(|error| error.to_string())? {
        None => Ok(()),
        Some(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub fn usb_resume(key: &str, position: Option<u64>) -> Result<Option<u64>, Failure> {
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err(Failure::CommandFailed); }
    let command = position.map_or_else(|| format!("--cineusb-resume {key}"), |seconds| format!("--cineusb-resume {key} {seconds}"));
    let reply = run_usb_helper(&command)?;
    if reply.is_empty() { Ok(None) } else { reply.parse().map(Some).map_err(|_| Failure::CommandFailed) }
}

#[cfg(target_os = "linux")]
pub fn usb_preferences() -> Result<String, Failure> {
    run_usb_helper("--cineusb-preferences")
}

#[cfg(target_os = "linux")]
pub fn save_usb_preferences(value: &str) -> Result<(), Failure> {
    if !value.bytes().all(|byte| matches!(byte, b'a' | b'c' | b'e' | b's' | b'0'..=b'2' | b' ')) {
        return Err(Failure::CommandFailed);
    }
    if run_usb_helper(&format!("--cineusb-preferences {value}"))? == value {
        Ok(())
    } else {
        Err(Failure::CommandFailed)
    }
}

#[cfg(target_os = "linux")]
fn usb_mutation(operation: &str, path: &Path, mount_id: u64) -> Result<(), Failure> {
    let hex = usb_path_hex(path)?;
    if run_usb_helper(&format!("{operation} {mount_id} '{hex}'"))? == "CINEUSB_USB_OK" {
        Ok(())
    } else {
        Err(Failure::CommandFailed)
    }
}

fn device_readable(path: &str) -> bool {
    std::ffi::CString::new(path)
        .map(|p| unsafe { libc::access(p.as_ptr(), libc::R_OK) } == 0)
        .unwrap_or(false)
}

fn valid_id(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix(plx_base::paths::STABLE_APP_ID) else {
        return false;
    };
    (suffix.is_empty() || (suffix.starts_with('.') && suffix.len() > 1))
        && suffix
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn validated_install<'a>(dir: &'a Path, id: &str) -> Option<&'a str> {
    if !valid_id(id) {
        return None;
    }
    let path = dir.to_str()?;
    let dev = format!("/media/developer/apps/usr/palm/applications/{id}");
    let homebrew = format!("/media/cryptofs/apps/usr/palm/applications/{id}");
    (path == dev || path == homebrew).then_some(path)
}

fn command(dir: &Path, id: &str) -> Result<String, Failure> {
    let dir = validated_install(dir, id).ok_or(Failure::Unsupported)?;
    // All interpolated bytes passed the strict id/path grammar above. Single quotes therefore
    // quote data rather than admitting shell syntax.
    Ok(format!(
        "if [ \"$(id -u)\" != 0 ]; then printf '{NOT_ROOT_MARKER}'; elif [ ! -c /dev/rtkmem ]; then exit 74; elif /usr/bin/jailer -t native -p '{dir}' -i '{id}' /bin/true >/dev/null 2>&1 && [ -c '/var/palm/jail/{id}/dev/rtkmem' ]; then printf '{OK_MARKER}'; else exit 74; fi"
    ))
}

fn repair(
    exec: impl FnOnce(&str) -> Result<String, Failure>,
    dir: &Path,
    id: &str,
    readable: impl FnOnce() -> bool,
) -> Result<(), Failure> {
    let command = command(dir, id)?;
    let payload = serde_json::json!({ "command": command }).to_string();
    let reply = exec(&payload)?;
    let value: Value = serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)?;
    if value.get("returnValue").and_then(Value::as_bool) != Some(true) {
        if value.get("errorCode").and_then(Value::as_i64) == Some(-1)
            && value.get("errorText").and_then(Value::as_str) == Some(HBC_ABSENT_TEXT)
        {
            return Err(Failure::HbcUnavailable);
        }
        return Err(Failure::CommandFailed);
    }
    match value.get("stdoutString").and_then(Value::as_str) {
        Some(OK_MARKER) if readable() => Ok(()),
        Some(OK_MARKER) => Err(Failure::Unreadable),
        Some(NOT_ROOT_MARKER) => Err(Failure::NotRoot),
        _ => Err(Failure::CommandFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(id: &str) -> PathBuf {
        PathBuf::from(format!("/media/developer/apps/usr/palm/applications/{id}"))
    }
    fn reply(stdout: &str) -> String {
        serde_json::json!({"returnValue": true, "stdoutString": stdout, "stdoutBytes": "", "stderrString": "", "stderrBytes": ""}).to_string()
    }

    #[test]
    fn only_exact_success_marker_plus_fresh_local_read_is_success() {
        let id = plx_base::paths::STABLE_APP_ID;
        assert_eq!(
            repair(|_| Ok(reply(OK_MARKER)), &dir(id), id, || true),
            Ok(())
        );
        assert_eq!(
            repair(|_| Ok(reply(OK_MARKER)), &dir(id), id, || false),
            Err(Failure::Unreadable)
        );
        for output in [
            "",
            "ok",
            "PLXNATIVE_JAIL_REPAIR_OK_74\n",
            "PLXNATIVE_JAIL_REPAIR_OK_74 extra",
        ] {
            assert_eq!(
                repair(|_| Ok(reply(output)), &dir(id), id, || true),
                Err(Failure::CommandFailed)
            );
        }
    }

    #[test]
    fn service_errors_malformed_replies_and_no_root_fail_closed() {
        let id = plx_base::paths::STABLE_APP_ID;
        assert_eq!(
            repair(|_| Err(Failure::Timeout), &dir(id), id, || true),
            Err(Failure::Timeout)
        );
        for response in ["garbage".to_string(), "{}".to_string(), serde_json::json!({"returnValue": false, "errorText": "secret", "stdoutString": OK_MARKER}).to_string()] {
            assert_eq!(repair(|_| Ok(response), &dir(id), id, || true), Err(Failure::CommandFailed));
        }
        let absent = serde_json::json!({
            "returnValue": false,
            "errorCode": -1,
            "errorText": HBC_ABSENT_TEXT,
        })
        .to_string();
        assert_eq!(
            repair(|_| Ok(absent), &dir(id), id, || true),
            Err(Failure::HbcUnavailable)
        );
        let command_error = serde_json::json!({
            "returnValue": false,
            "errorCode": -1,
            "errorText": "Command failed: /usr/bin/jailer",
        })
        .to_string();
        assert_eq!(
            repair(|_| Ok(command_error), &dir(id), id, || true),
            Err(Failure::CommandFailed)
        );
        assert_eq!(
            repair(|_| Ok(reply(NOT_ROOT_MARKER)), &dir(id), id, || true),
            Err(Failure::NotRoot)
        );
    }

    #[test]
    fn unsafe_identity_or_install_path_never_reaches_the_executor() {
        let called = std::cell::Cell::new(false);
        for (path, id) in [
            (dir("com.evil"), "com.evil"),
            (dir("com.beb.plxnative.bad;id"), "com.beb.plxnative.bad;id"),
            (
                PathBuf::from("/tmp/com.beb.plxnative"),
                plx_base::paths::STABLE_APP_ID,
            ),
            (
                PathBuf::from(
                    "/media/developer/apps/usr/palm/applications/com.beb.plxnative.debug",
                ),
                plx_base::paths::STABLE_APP_ID,
            ),
        ] {
            assert_eq!(
                repair(
                    |_| {
                        called.set(true);
                        Ok(reply(OK_MARKER))
                    },
                    &path,
                    id,
                    || true
                ),
                Err(Failure::Unsupported)
            );
        }
        assert!(!called.get());
    }

    #[test]
    fn command_is_fixed_and_contains_the_only_validated_install_arguments() {
        let id = "com.beb.plxnative.debug-1";
        let c = command(&dir(id), id).unwrap();
        assert!(c.contains("/usr/bin/jailer -t native -p '/media/developer/apps/usr/palm/applications/com.beb.plxnative.debug-1' -i 'com.beb.plxnative.debug-1' /bin/true"));
        assert!(c.contains("id -u"));
        assert!(c.contains("[ ! -c /dev/rtkmem ]"));
        assert!(c.contains("/var/palm/jail/com.beb.plxnative.debug-1/dev/rtkmem"));
    }
}
