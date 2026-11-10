//! One-shot root entry. Open only a regular file under the system's USB directory.
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::{OsStrExt, OsStringExt}, fs::OpenOptionsExt, net::UnixStream};
use std::path::{Component, Path, PathBuf};

fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, "invalid USB request") }

fn decode_path(hex: &str) -> io::Result<PathBuf> {
    if hex.len() % 2 != 0 || hex.len() > 8192 { return Err(invalid()); }
    let bytes = hex.as_bytes().chunks_exact(2).map(|pair| {
        let digits = std::str::from_utf8(pair).map_err(|_| invalid())?;
        u8::from_str_radix(digits, 16).map_err(|_| invalid())
    }).collect::<io::Result<Vec<_>>>()?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn decode_mount(field: &[u8]) -> PathBuf {
    let mut bytes = Vec::new();
    let mut rest = field;
    while let Some((&byte, tail)) = rest.split_first() {
        if let [b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', after @ ..] = rest {
            bytes.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
            rest = after;
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    PathBuf::from(OsString::from_vec(bytes))
}

fn system_path(path: &Path) -> io::Result<PathBuf> {
    if path.starts_with("/tmp/usb") { return Ok(path.to_path_buf()); }
    let relative = path.strip_prefix("/tmp/cineusb-volumes").map_err(|_| invalid())?;
    let mut parts = relative.components();
    let Some(Component::Normal(alias)) = parts.next() else { return Err(invalid()) };
    let id = alias.to_str().and_then(|name| name.strip_prefix("usb-"))
        .and_then(|name| name.split_once('-')).map(|(id, _)| id)
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(invalid)?;
    let mounts = fs::read("/proc/self/mountinfo")?;
    let mount = mounts.split(|byte| *byte == b'\n').find_map(|line| {
        let mut fields = line.split(|byte| *byte == b' ');
        (fields.next()? == id.as_bytes()).then(|| decode_mount(fields.nth(3).unwrap_or_default()))
    }).ok_or_else(invalid)?;
    if !mount.starts_with("/tmp/usb") || mount == Path::new("/tmp/usb") { return Err(invalid()); }
    Ok(mount.join(parts.as_path()))
}

fn open_under(usb: &Path, path: &Path) -> io::Result<File> {
    let relative = path.strip_prefix(usb).map_err(|_| invalid())?;
    let mut parts = relative.components().peekable();
    let mut dir = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(usb)?;
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else { return Err(invalid()) };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
        let directory = parts.peek().is_some();
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let flags = if directory { flags | libc::O_DIRECTORY } else { flags };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_fd(fd) };
        if directory {
            dir = file;
        } else {
            if !file.metadata()?.is_file() { return Err(invalid()); }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, 0) } < 0 { return Err(io::Error::last_os_error()); }
            return Ok(file);
        }
    }
    Err(invalid())
}

pub fn run(pid: &str, slot: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 || pid.parse::<u32>().ok().filter(|pid| *pid > 1).is_none()
        || slot.len() != 6 || !slot.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    let path = system_path(&decode_path(path_hex)?)?;
    let file = open_under(Path::new("/tmp/usb"), &path)?;
    let socket = UnixStream::connect(format!("/proc/{pid}/root/tmp/cineusb-fd-{slot}/file.sock"))?;
    crate::usb_fd::send(&socket, &file)?;
    io::stdout().write_all(b"CINEUSB_FD_OK")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confines_the_root_open_and_preserves_non_ascii_names() {
        let root = std::env::temp_dir().join(format!("cineusb-root-open-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let video = root.join("Película.mkv");
        fs::write(&video, b"movie").unwrap();
        let link = root.join("link.mkv");
        std::os::unix::fs::symlink(&video, &link).unwrap();
        assert_eq!(open_under(&root, &video).unwrap().metadata().unwrap().len(), 5);
        assert!(open_under(&root, &link).is_err());
        assert!(open_under(&root, &root.join("../elsewhere.mkv")).is_err());
        assert!(open_under(&root, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
