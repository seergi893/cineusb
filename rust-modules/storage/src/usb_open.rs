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

fn live_mounts(table: &[u8]) -> Vec<(u64, PathBuf)> {
    let mut mounts: Vec<_> = table.split(|byte| *byte == b'\n').filter_map(|line| {
        let mut fields = line.split(|byte| *byte == b' ');
        let id = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
        if fields.nth(2)? != b"/" { return None; }
        let path = decode_mount(fields.next()?);
        let relative = path.strip_prefix("/tmp/usb").ok()?;
        if relative.components().count() == 0 { return None; }
        let mut tail = fields.skip_while(|field| *field != b"-").skip(1);
        let filesystem = tail.next()?;
        let source = tail.next()?;
        (!matches!(filesystem, b"tmpfs" | b"ramfs" | b"autofs") && source.starts_with(b"/dev/"))
            .then_some((id, path))
    }).collect();
    mounts.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
    mounts.dedup_by(|a, b| a.1 == b.1);
    mounts
}

fn usb_mounts() -> io::Result<Vec<(u64, PathBuf)>> {
    Ok(live_mounts(&fs::read("/proc/self/mountinfo")?))
}

fn hex_path(path: &std::ffi::OsStr) -> String {
    use std::fmt::Write as _;
    let mut hex = String::new();
    for byte in path.as_bytes() { let _ = write!(hex, "{byte:02x}"); }
    hex
}

fn require_mount(path: &Path, mount_id: u64) -> io::Result<()> {
    if usb_mounts()?.iter().any(|(id, root)| *id == mount_id && path.starts_with(root)) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, "USB volume is no longer mounted"))
    }
}

pub fn list_volumes() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let volumes: Vec<_> = usb_mounts()?.into_iter().map(|(id, path)| (id, hex_path(path.as_os_str()))).collect();
    serde_json::to_writer(io::stdout().lock(), &volumes)?;
    Ok(())
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

fn open_under(usb: &Path, path: &Path, want_directory: bool) -> io::Result<File> {
    let relative = path.strip_prefix(usb).map_err(|_| invalid())?;
    let mut parts = relative.components().peekable();
    let mut dir = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(usb)?;
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else { return Err(invalid()) };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
        let directory = parts.peek().is_some();
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let flags = if directory || want_directory { flags | libc::O_DIRECTORY } else { flags };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_fd(fd) };
        if directory {
            dir = file;
        } else if want_directory {
            return Ok(file);
        } else {
            if !file.metadata()?.is_file() { return Err(invalid()); }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, 0) } < 0 { return Err(io::Error::last_os_error()); }
            return Ok(file);
        }
    }
    Err(invalid())
}

pub fn list_folder(mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let path = decode_path(path_hex)?;
    let mount_id = mount_id.parse().map_err(|_| invalid())?;
    require_mount(&path, mount_id)?;
    let folder = open_under(Path::new("/tmp/usb"), &path, true)?;
    let entries = folder_entries(&folder)?;
    require_mount(&path, mount_id)?;
    serde_json::to_writer(io::stdout().lock(), &entries)?;
    Ok(())
}

fn folder_entries(folder: &File) -> io::Result<Vec<(String, bool)>> {
    let mut entries = Vec::new();
    let mut bytes = 0;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", folder.as_raw_fd()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_file() {
            let name = hex_path(&entry.file_name());
            bytes += name.len() + 14;
            if bytes > 192 * 1024 { return Err(io::Error::other("USB folder listing too large")); }
            entries.push((name, kind.is_dir()));
        }
    }
    Ok(entries)
}

pub fn run(pid: &str, slot: &str, mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 || pid.parse::<u32>().ok().filter(|pid| *pid > 1).is_none()
        || slot.len() != 6 || !slot.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    let path = system_path(&decode_path(path_hex)?)?;
    let mount_id = mount_id.parse::<u64>().map_err(|_| invalid())?;
    if mount_id != 0 { require_mount(&path, mount_id)?; }
    let file = open_under(Path::new("/tmp/usb"), &path, false)?;
    if mount_id != 0 { require_mount(&path, mount_id)?; }
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
        assert_eq!(open_under(&root, &video, false).unwrap().metadata().unwrap().len(), 5);
        assert!(open_under(&root, &link, false).is_err());
        assert!(open_under(&root, &root.join("../elsewhere.mkv"), false).is_err());
        assert!(open_under(&root, &root, false).is_err());
        let films = root.join("Películas");
        fs::create_dir(&films).unwrap();
        fs::rename(&video, films.join(video.file_name().unwrap())).unwrap();
        let folder = open_under(&root, &films, true).unwrap();
        assert_eq!(folder_entries(&folder).unwrap(), [(hex_path(video.file_name().unwrap()), false)]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_mount_list_ignores_the_stale_sda1_placeholder() {
        let table = b"45 54 0:20 / /tmp/usb/sda/sda1 rw - tmpfs tmpfs rw\n365 54 8:17 / /tmp/usb/sdb/sdb1 rw,relatime - tntfs /dev/sdb1 rw\n";
        assert_eq!(live_mounts(table), [(365, PathBuf::from("/tmp/usb/sdb/sdb1"))]);
    }
}
