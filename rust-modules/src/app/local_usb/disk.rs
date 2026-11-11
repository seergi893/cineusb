//! Read-only access to mounts visible to the application process.
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Volume {
    pub path: PathBuf,
    pub label: Option<OsString>,
    pub mount_id: Option<u64>,
    pub system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Folder,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub name: OsString,
    pub kind: Kind,
    pub modified: Option<i64>,
    pub size: Option<u64>,
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Scan {
    pub volumes: Vec<Volume>,
    pub hidden_mounts: bool,
    pub system: bool,
}

pub(super) fn volumes() -> Scan {
    #[cfg(feature = "hostsim")]
    if let Some(path) = std::env::var_os("CINEUSB_SIM_ROOT").map(PathBuf::from) {
        return Scan { hidden_mounts: false, system: false, volumes: path
            .is_dir()
            .then_some(Volume {
                path,
                label: None,
                mount_id: None,
                system: false,
            })
            .into_iter()
            .collect() };
    }
    let usb = Path::new("/tmp/usb");
    let bridge = Path::new("/tmp/cineusb-volumes");
    scan(usb, bridge, fs::read("/proc/self/mountinfo"))
}

fn scan(usb: &Path, bridge: &Path, table: io::Result<Vec<u8>>) -> Scan {
    let mut mounts = table
        .as_deref()
        .map(|bytes| mounted_volumes(&bytes, usb, bridge))
        .unwrap_or_default();
    let mut hidden_mounts = mounts.iter().any(|volume| fs::read_dir(&volume.path).is_err());
    mounts.retain(|volume| fs::read_dir(&volume.path).is_ok());
    // A jail may contain the partition's directory without its filesystem. Compare device
    // numbers as a fallback when the mount table is absent or hides the jail's prefix.
    for path in subdirectories(usb).into_iter()
        .flat_map(|device| subdirectories(&device))
        .chain(subdirectories(bridge))
    {
        if mounts.iter().any(|volume| volume.path == path) {
            continue;
        }
        if on_different_device(&path) && fs::read_dir(&path).is_ok() {
            mounts.push(Volume { path, label: None, mount_id: None, system: false });
        } else if path.starts_with(usb) {
            hidden_mounts = true;
        }
    }
    // Prefer the explicit, root-created read-only bridge over a jail-visible
    // directory that might list names but fail EIO on the actual file bytes.
    // Keep unbridged volumes as a fallback if the root service is unavailable.
    mounts.sort_by(|a, b| {
        b.path.starts_with(bridge).cmp(&a.path.starts_with(bridge))
            .then_with(|| a.path.cmp(&b.path))
    });
    mounts.dedup_by(|a, b| a.path == b.path);
    Scan { volumes: mounts, hidden_mounts, system: false }
}

fn on_different_device(path: &Path) -> bool {
    path.parent()
        .and_then(|parent| fs::metadata(parent).ok())
        .zip(fs::metadata(path).ok())
        .is_some_and(|(parent, child)| parent.dev() != child.dev())
}

fn subdirectories(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

fn mounted_volumes(bytes: &[u8], usb: &Path, bridge: &Path) -> Vec<Volume> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|byte| *byte == b' ');
            let mount_id = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            let path = mount_path(fields.nth(3)?);
            let fs_type = line.split(|byte| *byte == b' ').skip_while(|field| *field != b"-").nth(1)?;
            let within = |root: &Path| path != root && path.starts_with(root);
            ((within(usb) || within(bridge)) && !matches!(fs_type, b"tmpfs" | b"ramfs"))
                .then_some(Volume {
                path,
                label: None,
                mount_id: Some(mount_id),
                    system: false,
            })
        })
        .collect()
}

fn mount_path(field: &[u8]) -> PathBuf {
    let mut decoded = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, tail)) = rest.split_first() {
        if let [b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', after @ ..] = rest {
            decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
            rest = after;
        } else {
            decoded.push(byte);
            rest = tail;
        }
    }
    PathBuf::from(OsString::from_vec(decoded))
}

pub(super) fn listing(path: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() || file_type.is_file() {
            if let Some(mut media) = media_entry(entry.file_name(), file_type.is_dir()) {
                let metadata = entry.metadata().ok();
                media.modified = metadata.as_ref().map(MetadataExt::mtime);
                media.size = metadata.filter(|_| file_type.is_file()).map(|meta| meta.len());
                entries.push(media);
            }
        }
    }
    sort_entries(&mut entries);
    Ok(entries)
}

pub(super) fn media_entry(name: OsString, directory: bool) -> Option<Entry> {
    let kind = if directory && !is_system_folder(&name) {
        Kind::Folder
    } else if !directory && is_video(Path::new(&name).extension()) {
        Kind::Video
    } else {
        return None;
    };
    Some(Entry { name, kind, modified: None, size: None })
}

pub(super) fn sort_entries(entries: &mut [Entry]) {
    sort_entries_by(entries, 0);
}

pub(super) fn sort_entries_by(entries: &mut [Entry], order: usize) {
    entries.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| match order {
        1 => b.modified.cmp(&a.modified),
        2 => b.size.cmp(&a.size),
        _ => std::cmp::Ordering::Equal,
    }).then_with(|| natural(a.name.as_encoded_bytes(), b.name.as_encoded_bytes()))
      .then_with(|| a.name.cmp(&b.name)));
}

pub(super) fn resort(entries: &mut [Entry], selected: usize, order: usize) -> usize {
    let name = entries.get(selected).map(|entry| entry.name.clone());
    sort_entries_by(entries, order);
    name.and_then(|name| entries.iter().position(|entry| entry.name == name)).unwrap_or_default()
}

fn natural(mut a: &[u8], mut b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    while let (Some(&x), Some(&y)) = (a.first(), b.first()) {
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let n = a.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let m = b.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let left = &a[a[..n].iter().take_while(|&&byte| byte == b'0').count()..n];
            let right = &b[b[..m].iter().take_while(|&&byte| byte == b'0').count()..m];
            let comparison = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
            if comparison != Ordering::Equal { return comparison; }
            a = &a[n..];
            b = &b[m..];
        } else {
            let comparison = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
            if comparison != Ordering::Equal { return comparison; }
            a = &a[1..];
            b = &b[1..];
        }
    }
    a.len().cmp(&b.len())
}

fn is_system_folder(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        ["$RECYCLE.BIN", "SYSTEM VOLUME INFORMATION", "LG SMART TV"]
            .iter()
            .any(|system| name.eq_ignore_ascii_case(system))
    })
}

fn is_video(extension: Option<&OsStr>) -> bool {
    extension.and_then(OsStr::to_str).is_some_and(|extension| {
        ["mkv", "mp4", "m4v", "mov", "webm", "ts", "m2ts", "mts"]
            .iter()
            .any(|kind| extension.eq_ignore_ascii_case(kind))
    })
}

/// Open a regular file read-only without probing the final byte first.
/// In particular, webOS's NTFS driver can return EIO on a tail read of a large
/// sparse/mapped MKV even though the demuxer can access the actual media ranges.
/// FFmpeg's normal probe and playback reads remain responsible for reporting
/// genuine I/O errors: do not mask them here.
pub(super) fn open_video(path: &Path) -> io::Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    // O_NONBLOCK protects the initial open if the directory entry was replaced by a FIFO.
    // The regular file is now pinned by its descriptor. Give FFmpeg blocking reads on it.
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn sort_keeps_folders_first_and_uses_natural_order_for_ties() {
        let mut entries = vec![
            media_entry("Episodio 10.mkv".into(), false).unwrap(),
            media_entry("Episodio 2.mkv".into(), false).unwrap(),
            media_entry("Temporada".into(), true).unwrap(),
        ];
        entries[0].size = Some(100);
        entries[0].modified = Some(10);
        entries[1].modified = Some(20);
        sort_entries_by(&mut entries, 0);
        assert_eq!(entries[0].kind, Kind::Folder);
        assert_eq!(entries[1].name, "Episodio 2.mkv");
        sort_entries_by(&mut entries, 1);
        assert_eq!(entries[1].name, "Episodio 2.mkv");
        assert_eq!(resort(&mut entries, 1, 2), 2);
        assert_eq!(entries[1].name, "Episodio 10.mkv");
    }

    #[test]
    fn mount_table_keeps_usb_partitions_and_decodes_paths() {
        let bytes = br"1 2 0:1 / /tmp/usb rw - tmpfs none rw
12 1 8:1 / /tmp/usb/sda/Mis\040pelis rw - ntfs /dev/sda1 rw
13 1 8:2 / /tmp/usb-backup/sdb rw - ntfs /dev/sdb rw
14 1 8:1 / /tmp/cineusb-volumes/sda1 ro - ntfs /dev/sda1 rw
15 1 0:1 / /tmp/usb/sda/sda rw - tmpfs none rw
";
        let mounts = mounted_volumes(
            bytes,
            Path::new("/tmp/usb"),
            Path::new("/tmp/cineusb-volumes"),
        );
        assert_eq!(
            mounts,
            [
                Volume {
                    path: "/tmp/usb/sda/Mis pelis".into(),
                    label: None,
                    mount_id: Some(12),
                    system: false,
                },
                Volume {
                    path: "/tmp/cineusb-volumes/sda1".into(),
                    label: None,
                    mount_id: Some(14),
                    system: false,
                },
            ]
        );
    }

    #[test]
    fn read_check_seeks_past_four_gib() {
        let dir = plx_base::testscratch::process_dir("cineusb-large-file");
        let path = dir.join("large.mkv");
        let size = (1u64 << 32) + 1;
        fs::File::create(&path).unwrap().set_len(size).unwrap();
        assert_eq!(open_video(&path).unwrap().metadata().unwrap().len(), size);
        plx_base::testscratch::remove_tree(&dir);
    }

    #[test]
    fn listing_keeps_real_names_and_ignores_links_and_other_formats() {
        let path = plx_base::testscratch::process_dir("cineusb-listing");
        fs::create_dir(path.join("Sèries")).unwrap();
        for name in ["$RECYCLE.BIN", "System Volume Information", "LG Smart TV"] {
            fs::create_dir(path.join(name)).unwrap();
        }
        // Do not hide unrelated hidden or similarly named user folders.
        fs::create_dir(path.join(".personal")).unwrap();
        fs::create_dir(path.join("LG Smart TV Films")).unwrap();
        fs::write(path.join("Película 01.MKV"), b"media").unwrap();
        fs::write(path.join("Película 01.srt"), b"caption").unwrap();
        std::os::unix::fs::symlink("Película 01.MKV", path.join("alias.mp4")).unwrap();
        assert_eq!(
            listing(&path).unwrap().into_iter().map(|mut entry| {
                assert!(entry.modified.is_some());
                assert_eq!(entry.size, (entry.kind == Kind::Video).then_some(5));
                entry.modified = None;
                entry.size = None;
                entry
            }).collect::<Vec<_>>(),
            [
                Entry {
                    name: ".personal".into(),
                    kind: Kind::Folder, modified: None, size: None
                },
                Entry {
                    name: "LG Smart TV Films".into(),
                    kind: Kind::Folder, modified: None, size: None
                },
                Entry {
                    name: "Sèries".into(),
                    kind: Kind::Folder, modified: None, size: None
                },
                Entry {
                    name: "Película 01.MKV".into(),
                    kind: Kind::Video, modified: None, size: None
                },
            ]
        );
        assert!(open_video(&path.join("alias.mp4")).is_err());
        assert_eq!(open_video(&path.join("Película 01.MKV")).unwrap().metadata().unwrap().len(), 5);
        plx_base::testscratch::remove_tree(&path);
    }

    #[test]
    fn jail_placeholder_is_never_an_empty_volume() {
        let dir = plx_base::testscratch::process_dir("cineusb-placeholder");
        let usb = dir.join("usb");
        fs::create_dir_all(usb.join("sda/sda1")).unwrap();
        let result = scan(&usb, &dir.join("bridge"), Ok(Vec::new()));
        assert!(result.volumes.is_empty());
        assert!(result.hidden_mounts);
        plx_base::testscratch::remove_tree(&dir);
    }
}
