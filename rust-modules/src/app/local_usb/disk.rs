//! Read-only access to mounts visible to the application process.
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Volume {
    pub path: PathBuf,
    pub mount_id: Option<u64>,
}

impl Volume {
    #[cfg(not(feature = "hostsim"))]
    pub fn bridged(&self) -> bool {
        self.path.starts_with("/tmp/cineusb-volumes")
    }
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
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Scan {
    pub volumes: Vec<Volume>,
    pub hidden_mounts: bool,
}

pub(super) fn volumes() -> Scan {
    #[cfg(feature = "hostsim")]
    if let Some(path) = std::env::var_os("CINEUSB_SIM_ROOT").map(PathBuf::from) {
        return Scan { hidden_mounts: false, volumes: path
            .is_dir()
            .then_some(Volume {
                path,
                mount_id: None,
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
            mounts.push(Volume { path, mount_id: None });
        } else if path.starts_with(usb) {
            hidden_mounts = true;
        }
    }
    mounts.sort_by(|a, b| a.path.cmp(&b.path));
    mounts.dedup_by(|a, b| a.path == b.path);
    Scan { volumes: mounts, hidden_mounts }
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
                mount_id: Some(mount_id),
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
        let name = entry.file_name();
        let kind = if file_type.is_dir() {
            Kind::Folder
        } else if file_type.is_file() && is_video(Path::new(&name).extension()) {
            Kind::Video
        } else {
            continue;
        };
        entries.push(Entry { name, kind });
    }
    entries.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

fn is_video(extension: Option<&OsStr>) -> bool {
    extension.and_then(OsStr::to_str).is_some_and(|extension| {
        ["mkv", "mp4", "m4v", "mov", "webm", "ts", "m2ts", "mts"]
            .iter()
            .any(|kind| extension.eq_ignore_ascii_case(kind))
    })
}

/// Check that the app can open, read and seek on the selected file under its own permissions.
pub(super) fn open_video(path: &Path) -> io::Result<fs::File> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut byte)?;
    file.rewind()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    mount_id: Some(12)
                },
                Volume {
                    path: "/tmp/cineusb-volumes/sda1".into(),
                    mount_id: Some(14)
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
        fs::write(path.join("Película 01.MKV"), b"media").unwrap();
        fs::write(path.join("Película 01.srt"), b"caption").unwrap();
        std::os::unix::fs::symlink("Película 01.MKV", path.join("alias.mp4")).unwrap();
        assert_eq!(
            listing(&path).unwrap(),
            [
                Entry {
                    name: "Sèries".into(),
                    kind: Kind::Folder
                },
                Entry {
                    name: "Película 01.MKV".into(),
                    kind: Kind::Video
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
