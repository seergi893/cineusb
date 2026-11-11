//! CineUSB's install-scoped settings, independent of Plex authentication.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const ROOT: &str = "/var/lib/cineusb";

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid CineUSB preferences")
}

fn record(language: &str, forced: &str, audio: &str) -> io::Result<String> {
    if !matches!(language, "es" | "ca") || !matches!(forced, "0" | "1") || !matches!(audio, "0" | "1") {
        return Err(invalid());
    }
    Ok(format!("{language} {forced} {audio}"))
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
    file.take(64).read_to_string(&mut text)?;
    if text.split(' ').count() == 7 { return validate_extended(&text).map(|()| Some(text)); }
    let (lang, options) = text.split_once(' ').ok_or_else(invalid)?;
    let (forced, audio) = options.split_once(' ').unwrap_or((options, "0"));
    record(lang, forced, audio).map(Some)
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

fn validate_extended(text: &str) -> io::Result<()> {
    let fields: Vec<_> = text.split(' ').collect();
    let [language, forced, audio, next, accent, cards, sort] = fields.as_slice() else { return Err(invalid()) };
    record(language, forced, audio)?;
    if !matches!(*next, "0" | "1") || ![accent, cards, sort].into_iter().all(|value| matches!(*value, "0" | "1" | "2")) {
        return Err(invalid());
    }
    Ok(())
}

pub fn write_extended(fields: &[String]) -> io::Result<()> {
    let text = fields.join(" ");
    validate_extended(&text)?;
    save(&preference_file()?, &text)?;
    io::stdout().write_all(text.as_bytes())
}

/// Store a separate atomic record per video. Keep the full key to verify the index on read.
pub fn resume(key: &str, position: Option<&str>) -> io::Result<()> {
    if key.is_empty() || key.len() > 16_384 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |state, byte|
        (state ^ u64::from(byte)).wrapping_mul(0x100000001b3));
    let prefs = preference_file()?;
    let install = prefs.file_name().ok_or_else(invalid)?.to_string_lossy();
    let path = prefs.with_file_name(format!("{install}.resume-{hash:016x}"));
    if position.is_none() || position == Some("0") {
        match open_directory(path.parent().ok_or_else(invalid)?, false) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => { result?; }
        }
    }
    if let Some(position) = position {
        let position = position.parse::<u64>().map_err(|_| invalid())?;
        if position == 0 {
            match fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                result => return result,
            }
        }
        return save(&path, &format!("{key} {position}"));
    }
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() { return Err(invalid()); }
    let mut record = String::new();
    file.take(16_416).read_to_string(&mut record)?;
    if let Some((saved, position)) = record.split_once(' ') {
        if saved == key {
            write!(io::stdout().lock(), "{}", position.parse::<u64>().map_err(|_| invalid())?)?;
        }
    }
    Ok(())
}

pub fn read() -> io::Result<()> {
    if let Some(text) = load(&preference_file()?)? {
        io::stdout().write_all(text.as_bytes())?;
    }
    Ok(())
}

pub fn write(language: &str, forced: &str, audio: &str) -> io::Result<()> {
    let text = record(language, forced, audio)?;
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
        save(&path, "ca 1").unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some("ca 1 0"));
        save(&path, &record("es", "1", "1").unwrap()).unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some("es 1 1"));
        let extended = "ca 1 0 0 2 1 2";
        assert!(validate_extended(extended).is_ok());
        save(&path, extended).unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some(extended));
        assert!(validate_extended("ca 1 0 1 3 0 0").is_err());
        assert!(record("ca", "yes", "0").is_err());
        assert!(record("../es", "0", "0").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
