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
        let id = std::str::from_utf8(fields.next()?).ok()?.parse::<u64>().ok()?;
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
    let labels: Vec<_> = fs::read_dir("/dev/disk/by-label").into_iter().flatten().filter_map(|entry| {
        let entry = entry.ok()?;
        let device = fs::canonicalize(entry.path()).ok()?;
        Some((device, entry.file_name()))
    }).collect();
    let volumes: Vec<_> = usb_mounts()?.into_iter().map(|(id, path)| {
        let label = path.file_name().and_then(|name| {
            let device = Path::new("/dev").join(name);
            labels.iter().find(|(path, _)| *path == device).map(|(_, label)| hex_path(label))
        });
        (id, hex_path(path.as_os_str()), label)
    }).collect();
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
        // Plain openat does not opt into large files on 32-bit webOS.
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_LARGEFILE;
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

pub fn volume_details(mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let id = mount_id.parse().map_err(|_| invalid())?;
    let path = decode_path(path_hex)?;
    if !usb_mounts()?.contains(&(id, path.clone())) { return Err(invalid()); }
    let folder = open_under(Path::new("/tmp/usb"), &path, true)?;
    if fd_mount_id(&folder)? != id { return Err(invalid()); }
    let mut stats = std::mem::MaybeUninit::<libc::statvfs64>::uninit();
    let capacity = if unsafe { libc::fstatvfs64(folder.as_raw_fd(), stats.as_mut_ptr()) } == 0 {
        let stats = unsafe { stats.assume_init() };
        let unit = u64::from(stats.f_frsize);
        let total = stats.f_blocks.checked_mul(unit).filter(|total| *total > 0);
        total.map(|total| (total, stats.f_blocks.saturating_sub(stats.f_bfree).saturating_mul(unit),
            stats.f_bavail.saturating_mul(unit).min(total), stats.f_flag & libc::ST_RDONLY != 0))
    } else { None };
    let table = fs::read("/proc/self/mountinfo")?;
    let device = table.split(|byte| *byte == b'\n').find(|line|
        line.split(|byte| *byte == b' ').next().is_some_and(|field| field == mount_id.as_bytes()))
        .and_then(mount_source).map(decode_mount).filter(|path| path.starts_with("/dev"));
    let uuid = device.as_deref().and_then(volume_identity);
    serde_json::to_writer(io::stdout().lock(), &(capacity, uuid))?;
    Ok(())
}

fn volume_identity(device: &Path) -> Option<String> {
    let device = fs::canonicalize(device).ok()?;
    let alias = |directory| fs::read_dir(directory).into_iter().flatten().filter_map(Result::ok)
        .filter_map(|entry| (fs::canonicalize(entry.path()).ok()? == device)
            .then(|| entry.file_name().into_string().ok()).flatten()).min();
    if let Some(uuid) = alias("/dev/disk/by-uuid") { return Some(format!("uuid:{uuid}")); }
    // Some webOS builds expose blkid without creating the device aliases.
    if let Ok(output) = std::process::Command::new("blkid").args(["-s", "UUID", "-o", "value"]).arg(&device).output() {
        if let Ok(uuid) = std::str::from_utf8(&output.stdout) {
            let uuid = uuid.trim();
            if output.status.success() && !uuid.is_empty() && uuid.len() <= 128
                && uuid.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
                return Some(format!("uuid:{uuid}"));
            }
        }
    }
    alias("/dev/disk/by-id").map(|id| format!("disk:{id}"))
}

fn fd_mount_id(file: &File) -> io::Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines().find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| io::Error::other("cannot verify USB mount for the opened file"))
}

/// Remove children relative to open directories. Never follow links or cross another mount,
/// including bind mounts that have the same device number.
fn remove_tree(parent: &File, name: &std::ffi::OsStr, mount: u64, depth: usize, deadline: std::time::Instant) -> io::Result<()> {
    if std::time::Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
    if depth >= 64 { return Err(io::Error::other("USB folder nesting exceeds the supported limit")); }
    let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    let entry = unsafe { File::from_raw_fd(fd) };
    if fd_mount_id(&entry)? != mount { return Err(invalid()); }
    let directory = entry.metadata()?.is_dir();
    if depth == 0 && !directory { return Err(invalid()); }
    if directory {
        for child in fs::read_dir(format!("/proc/self/fd/{}", entry.as_raw_fd()))? {
            remove_tree(&entry, &child?.file_name(), mount, depth + 1, deadline)?;
        }
    }
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remove_folder_under(root: &Path, path: &Path, mount: u64) -> io::Result<()> {
    if path == root { return Err(invalid()); }
    let parent = open_under(root, path.parent().ok_or_else(invalid)?, true)
        .or_else(|error| {
            if path.parent() != Some(root) { return Err(error); }
            OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(root)
        })?;
    if fd_mount_id(&parent)? != mount { return Err(invalid()); }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    remove_tree(&parent, path.file_name().ok_or_else(invalid)?, mount, 0, deadline)
}

pub fn delete_folder(mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let path = decode_path(path_hex)?;
    let id = mount_id.parse().map_err(|_| invalid())?;
    let root = usb_mounts()?.into_iter().find_map(|(mount, root)| {
        (mount == id && path != root && path.starts_with(&root)).then_some(root)
    }).ok_or_else(invalid)?;
    // Traverse the full path from /tmp/usb so no parent component can be a symlink.
    let relative = path.strip_prefix(&root).map_err(|_| invalid())?;
    if relative.components().count() < 2 || !relative.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(invalid());
    }
    let result = remove_folder_under(Path::new("/tmp/usb"), &path, id);
    let detail = result.err().map(|error| error.to_string());
    serde_json::to_writer(io::stdout().lock(), &detail)?;
    Ok(())
}

fn delete_under(root: &Path, path: &Path, mount_id: u64) -> io::Result<()> {
    let parent = open_under(root, path.parent().ok_or_else(invalid)?, true)?;
    let name = CString::new(path.file_name().ok_or_else(invalid)?.as_bytes()).map_err(|_| invalid())?;
    if fd_mount_id(&parent)? != mount_id { return Err(invalid()); }
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() || fd_mount_id(&file)? != mount_id { return Err(invalid()); }
    // Unlink relative to the verified parent. Even a last-moment symlink replacement cannot
    // follow its target, and flags=0 cannot unlink a directory.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn delete(mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let path = decode_path(path_hex)?;
    let id = mount_id.parse().map_err(|_| invalid())?;
    require_mount(&path, id)?;
    let is_video = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        ["mkv", "mp4", "m4v", "mov", "webm", "ts", "m2ts", "mts"]
            .iter().any(|video| ext.eq_ignore_ascii_case(video))
    });
    if !is_video { return Err(invalid()); }
    delete_under(Path::new("/tmp/usb"), &path, id)?;
    io::stdout().write_all(b"CINEUSB_USB_OK")
}

fn pdm(method: &str, payload: &serde_json::Value) -> io::Result<serde_json::Value> {
    let uri = format!("luna://com.webos.service.pdm/{method}");
    let output = std::process::Command::new("/usr/bin/luna-send")
        .args(["-n", "1", "-w", "4000", &uri, &payload.to_string()]).output()?;
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| io::Error::other("webOS did not return USB device status"))?;
    if !output.status.success() || response["returnValue"].as_bool() != Some(true) {
        let detail = response["errorText"].as_str().unwrap_or("webOS rejected the request");
        return Err(io::Error::other(format!("{detail} ({method}, {})", response["errorCode"])));
    }
    Ok(response)
}

fn pdm_device<'a>(status: &'a serde_json::Value, partition: &str) -> io::Result<(u64, Vec<&'a str>)> {
    let devices = status["deviceStatusList"].as_array().ok_or_else(invalid)?;
    let mut matches = devices.iter().filter_map(|device| {
        // Retail LG firmware can omit deviceType. The partition comes from a verified USB mount.
        if device["deviceType"].as_str().is_some_and(|kind| kind != "USB_STORAGE") { return None; }
        let drives: Vec<_> = device["driveStatusList"].as_array()?.iter()
            .filter_map(|drive| drive["driveName"].as_str()).collect();
        drives.contains(&partition).then(|| (device["deviceNum"].as_u64().filter(|n| *n > 0), drives))
    });
    let (Some(number), drives) = matches.next().ok_or_else(|| io::Error::other("USB disk not found in webOS"))?
        else { return Err(invalid()); };
    if matches.next().is_some() { return Err(invalid()); }
    Ok((number, drives))
}

fn mount_source(line: &[u8]) -> Option<&[u8]> {
    line.split(|byte| *byte == b' ').skip_while(|field| *field != b"-").nth(2)
}

fn eject_checked(mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 { return Err(invalid()); }
    let path = decode_path(path_hex)?;
    let id = mount_id.parse().map_err(|_| invalid())?;
    let table = fs::read("/proc/self/mountinfo")?;
    if !live_mounts(&table).iter().any(|(mount, root)| *mount == id && *root == path) {
        return Err(invalid());
    }
    let source = table.split(|byte| *byte == b'\n').find_map(|line| {
        let field = line.split(|byte| *byte == b' ').next()?;
        (std::str::from_utf8(field).ok()?.parse::<u64>().ok()? == id)
            .then(|| mount_source(line))?
    }).and_then(|source| source.strip_prefix(b"/dev/"))
        .and_then(|source| std::str::from_utf8(source).ok()).ok_or_else(invalid)?;
    let status = pdm("getAttachedDeviceStatus", &serde_json::json!({"subscribe": true}))?;
    let (device, drives) = pdm_device(&status, source)?;
    require_mount(&path, id)?;
    pdm("eject", &serde_json::json!({"deviceNum": device}))?;
    for _ in 0..5 {
        let table = fs::read("/proc/self/mountinfo")?;
        let mounted = table.split(|byte| *byte == b'\n').filter_map(mount_source)
            .filter_map(|device| device.strip_prefix(b"/dev/"))
            .any(|device| drives.iter().any(|drive| device == drive.as_bytes()));
        if !mounted { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Err(io::Error::other("webOS still shows partitions mounted after eject"))
}

pub fn eject(mount_id: &str, path_hex: &str) -> io::Result<()> {
    let error = eject_checked(mount_id, path_hex).err().map(|error| error.to_string());
    serde_json::to_writer(io::stdout().lock(), &error)?;
    Ok(())
}

type FolderEntry = (String, bool, Option<i64>, Option<u64>);

fn folder_entries(folder: &File) -> io::Result<Vec<FolderEntry>> {
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut wire_bytes = 0;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", folder.as_raw_fd()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_file() {
            let name = hex_path(&entry.file_name());
            bytes += name.len() + 14;
            wire_bytes += name.len() + 54;
            if bytes > 192 * 1024 || wire_bytes > 512 * 1024 { return Err(io::Error::other("USB folder listing too large")); }
            use std::os::unix::fs::MetadataExt;
            let metadata = entry.metadata().ok();
            let modified = metadata.as_ref().map(MetadataExt::mtime);
            let size = metadata.filter(|_| kind.is_file()).map(|meta| meta.len());
            entries.push((name, kind.is_dir(), modified, size));
        }
    }
    Ok(entries)
}

pub fn run(pid: &str, slot: &str, mount_id: &str, path_hex: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 || pid.parse::<u32>().ok().filter(|pid| *pid > 1).is_none()
        || slot.len() != 6 || !slot.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    let path = system_path(&decode_path(path_hex)?).map_err(|e| at_stage("resolve path", e))?;
    let mount_id = mount_id.parse::<u64>().map_err(|_| invalid())?;
    if mount_id != 0 { require_mount(&path, mount_id).map_err(|e| at_stage("check mount before open", e))?; }
    let file = open_under(Path::new("/tmp/usb"), &path, false).map_err(|e| at_stage("open file", e))?;
    if mount_id != 0 { require_mount(&path, mount_id).map_err(|e| at_stage("check mount after open", e))?; }
    let socket = UnixStream::connect(format!("/proc/{pid}/root/tmp/cineusb-fd-{slot}/file.sock"))
        .map_err(|e| at_stage("connect to app socket", e))?;
    crate::usb_fd::send(&socket, &file).map_err(|e| at_stage("send descriptor", e))?;
    io::stdout().write_all(b"CINEUSB_FD_OK")
}

fn at_stage(stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_delete_confines_the_unlink_to_the_selected_mount_and_regular_file() {
        let root = std::env::temp_dir().join(format!("cineusb-delete-{}", std::process::id()));
        let folder = root.join("Pel·lícules");
        fs::create_dir_all(&folder).unwrap();
        let video = folder.join("Pel·lícula.mkv");
        let neighbour = folder.join("Altres.mkv");
        fs::write(&video, b"video").unwrap();
        fs::write(&neighbour, b"keep").unwrap();
        let link = folder.join("Enllaç.mkv");
        std::os::unix::fs::symlink(&neighbour, &link).unwrap();
        let mount = fd_mount_id(&File::open(&folder).unwrap()).unwrap();
        assert!(delete_under(&root, &video, mount + 1).is_err());
        assert!(delete_under(&root, &link, mount).is_err());
        assert!(delete_under(&root, &folder.join("..").join("other.mkv"), mount).is_err());
        delete_under(&root, &video, mount).unwrap();
        assert!(!video.exists());
        assert!(link.is_symlink());
        assert_eq!(fs::read(neighbour).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn usb_recursive_delete_removes_only_the_selected_directory() {
        let root = std::env::temp_dir().join(format!("cineusb-tree-{}", std::process::id()));
        let selected = root.join("Selected");
        let nested = selected.join("Nested");
        fs::create_dir_all(&nested).unwrap();
        let neighbour = root.join("keep.txt");
        fs::write(&neighbour, b"keep").unwrap();
        fs::write(nested.join("notes.txt"), b"delete").unwrap();
        std::os::unix::fs::symlink(&root, selected.join("outside")).unwrap();
        let mount = fd_mount_id(&File::open(&root).unwrap()).unwrap();
        assert!(remove_folder_under(&root, &root, mount).is_err());
        assert!(remove_folder_under(&root, &selected, mount + 1).is_err());
        remove_folder_under(&root, &selected, mount).unwrap();
        assert!(!selected.exists());
        assert_eq!(fs::read(neighbour).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn usb_eject_maps_retail_lg_partitions_to_the_same_device() {
        let status = serde_json::json!({"deviceStatusList":[
            {"deviceNum":0,"driveStatusList":[{"driveName":"apps"}]},
            {"deviceNum":1,"deviceStatus":"NOTHING"},
            {"deviceNum":2,"driveStatusList":[{"driveName":"sda1"},{"driveName":"sda2"}]},
            {"deviceNum":3,"deviceType":"USB_STORAGE","driveStatusList":[{"driveName":"sdb1"}]}
        ],"returnValue":true});
        assert_eq!(pdm_device(&status, "sda1").unwrap(), (2, vec!["sda1", "sda2"]));
        assert_eq!(pdm_device(&status, "sdb1").unwrap(), (3, vec!["sdb1"]));
        assert!(pdm_device(&status, "apps").is_err());
        assert!(pdm_device(&status, "sdc1").is_err());
        assert_eq!(mount_source(b"365 54 8:17 / /tmp/usb/sdb/sdb1 rw - tntfs /dev/sdb1 rw"), Some(&b"/dev/sdb1"[..]));
    }

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
        let entries = folder_entries(&folder).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!((&entries[0].0, entries[0].1), (&hex_path(video.file_name().unwrap()), false));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_mount_list_ignores_the_stale_sda1_placeholder() {
        let table = b"45 54 0:20 / /tmp/usb/sda/sda1 rw - tmpfs tmpfs rw\n365 54 8:17 / /tmp/usb/sdb/sdb1 rw,relatime - tntfs /dev/sdb1 rw\n";
        assert_eq!(live_mounts(table), [(365, PathBuf::from("/tmp/usb/sdb/sdb1"))]);
    }

    #[test]
    fn usb_open_transfers_a_descriptor_for_a_video_over_four_gib() {
        use std::os::unix::fs::FileExt;

        let root = std::env::temp_dir().join(format!("cineusb-large-video-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("large.mkv");
        let offset = 4 * 1024 * 1024 * 1024;
        File::create(&path).unwrap().write_all_at(b"video", offset).unwrap();

        let original = open_under(&root, &path, false).unwrap();
        let (app, helper) = UnixStream::pair().unwrap();
        crate::usb_fd::send(&helper, &original).unwrap();
        let file = crate::usb_fd::receive(&app).unwrap();
        assert_eq!(file.metadata().unwrap().len(), offset + 5);
        let mut bytes = [0; 5];
        file.read_exact_at(&mut bytes, offset).unwrap();
        assert_eq!(&bytes, b"video");
        fs::remove_dir_all(root).unwrap();
    }
}
