//! CineUSB's install-scoped settings, independent of Plex authentication.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const ROOT: &str = "/var/lib/cineusb";

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid CineUSB preferences")
}

fn record(language: &str, forced: &str) -> io::Result<String> {
    if !matches!(language, "es" | "ca") || !matches!(forced, "0" | "1") {
        return Err(invalid());
    }
    Ok(format!("{language} {forced}"))
}

fn preference_file() -> io::Result<PathBuf> {
    if unsafe { libc::geteuid() } != 0 { return Err(io::ErrorKind::PermissionDenied.into()); }
    let executable = std::env::current_exe()?;
    let id = crate::runtime::app_identity(&executable).map_err(|_| invalid())?;
    Ok(Path::new(ROOT).join(id))
}

fn open_directory(path: &Path, create: bool) -> io::Result<File> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let dir = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?;
    let meta = dir.metadata()?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(dir)
}

fn load(path: &Path) -> io::Result<Option<String>> {
    match open_directory(path.parent().ok_or_else(invalid)?, false) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() { return Err(invalid()); }
    let mut text = String::new();
    file.take(5).read_to_string(&mut text)?;
    let (lang, forced) = text.split_once(' ').ok_or_else(invalid)?;
    record(lang, forced).map(Some)
}

fn save(path: &Path, text: &str) -> io::Result<()> {
    let parent = path.parent().ok_or_else(invalid)?;
    let dir = open_directory(parent, true)?;
    let mut random = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let temp = parent.join(format!(".settings-{suffix}"));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let result = file.write_all(text.as_bytes()).and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() { let _ = fs::remove_file(temp); }
    result?;
    dir.sync_all()
}

pub fn read() -> io::Result<()> {
    if let Some(text) = load(&preference_file()?)? {
        io::stdout().write_all(text.as_bytes())?;
    }
    Ok(())
}

pub fn write(language: &str, forced: &str) -> io::Result<()> {
    let text = record(language, forced)?;
    save(&preference_file()?, &text)?;
    io::stdout().write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_preferences_round_trip_preserves_both_settings() {
        let root = std::env::temp_dir().join(format!("cineusb-preferences-{}", std::process::id()));
        let path = root.join("settings");
        assert_eq!(load(&path).unwrap(), None);
        save(&path, &record("ca", "1").unwrap()).unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some("ca 1"));
        save(&path, &record("es", "1").unwrap()).unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some("es 1"));
        assert!(record("ca", "yes").is_err());
        assert!(record("../es", "0").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
