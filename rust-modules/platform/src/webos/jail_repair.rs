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
    registration
        .call(EXEC_URI, payload, BUDGET)
        .map_err(|failure| match failure {
            super::ls2::Fail::Timeout => Failure::Timeout,
            super::ls2::Fail::Setup { .. } => Failure::HbcUnavailable,
        })
}

#[cfg(any(feature = "hostsim", test, feature = "test-support"))]
fn call_hbc(_payload: &str) -> Result<String, Failure> {
    Err(Failure::HbcUnavailable)
}

/// Reconcile read-only USB mounts in this app's jail. No file name supplied by a disk is
/// interpolated into the shell program. The caller checks visibility in its own namespace.
pub fn sync_usb_volumes() -> Result<(), Failure> {
    let id = plx_base::paths::app_id();
    if !valid_id(id) { return Err(Failure::Unsupported); }
    let command = format!("app_id='{id}'\n{}", include_str!("usb_mounts.sh"));
    let reply = call_hbc(&serde_json::json!({ "command": command }).to_string())?;
    let reply: Value = serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)?;
    if reply.get("returnValue").and_then(Value::as_bool) == Some(true)
        && reply.get("stdoutString").and_then(Value::as_str) == Some("CINEUSB_MOUNTS_OK") {
        Ok(())
    } else {
        Err(Failure::CommandFailed)
    }
}

/// Ask the root service for one read-only file descriptor. The socket remains private to this
/// request, and the open file crosses the jail boundary without copying bytes or changing mounts.
#[cfg(target_os = "linux")]
pub fn open_usb_file(path: &Path) -> Result<std::fs::File, Failure> {
    use std::fmt::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::{ffi::OsStrExt, net::UnixListener};

    let id = plx_base::paths::app_id();
    let dir = validated_install(plx_base::paths::app_dir(), id).ok_or(Failure::Unsupported)?;
    let prefix = dir.strip_suffix(&format!("/applications/{id}")).ok_or(Failure::Unsupported)?;
    let helper = format!("{prefix}/services/{id}.storage/plxnative-storage");
    if !(path.starts_with("/tmp/usb") || path.starts_with("/tmp/cineusb-volumes")) {
        return Err(Failure::Unsupported);
    }
    let mut path_hex = String::new();
    for byte in path.as_os_str().as_bytes() { let _ = write!(path_hex, "{byte:02x}"); }
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
        let command = format!("exec '{helper}' --cineusb-open {} '{slot}' '{path_hex}'", std::process::id());
        let reply = call_hbc(&serde_json::json!({ "command": command }).to_string())?;
        let reply: Value = serde_json::from_str(&reply).map_err(|_| Failure::CommandFailed)?;
        if reply.get("returnValue").and_then(Value::as_bool) != Some(true)
            || reply.get("stdoutString").and_then(Value::as_str) != Some("CINEUSB_FD_OK") {
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
