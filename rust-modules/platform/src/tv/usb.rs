//! Offline USB operations through the installed TV port. No webOS types escape this interface.
use super::sandbox::Failure;

pub type Volume = (u64, std::path::PathBuf, Option<std::ffi::OsString>);
pub type Entry = (std::ffi::OsString, bool, Option<i64>, Option<u64>);
pub type Details = (Option<(u64, u64, u64, bool)>, Option<String>);

/// The privileged USB adapter. The absent port refuses operations without touching a filesystem.
pub trait Usb: Sync {
    fn open_file(&self, _path: &std::path::Path, _mount: Option<u64>) -> Result<std::fs::File, Failure> { Err(Failure::Unsupported) }
    fn volumes(&self) -> Result<Vec<Volume>, Failure> { Err(Failure::Unsupported) }
    fn list_folder(&self, _path: &std::path::Path, _mount: u64) -> Result<Vec<Entry>, Failure> { Err(Failure::Unsupported) }
    fn volume_details(&self, _path: &std::path::Path, _mount: u64) -> Result<Details, Failure> { Err(Failure::Unsupported) }
    fn delete_video(&self, _path: &std::path::Path, _mount: u64) -> Result<(), Failure> { Err(Failure::Unsupported) }
    fn eject_disk(&self, _path: &std::path::Path, _mount: u64) -> Result<(), String> { Err("USB platform unavailable".into()) }
    fn folder_stamps(&self, _paths: &[std::path::PathBuf], _mount: u64) -> Result<Vec<(u64, i64, i64)>, Failure> { Err(Failure::Unsupported) }
    fn delete_folder(&self, _path: &std::path::Path, _mount: u64) -> Result<(), String> { Err("USB platform unavailable".into()) }
    fn clear_history(&self, _base: &str) -> Result<(), Failure> { Err(Failure::Unsupported) }
    fn history(&self, _prefix: &str) -> Result<std::collections::BTreeMap<String, u64>, Failure> { Err(Failure::Unsupported) }
    fn resume(&self, _key: &str, _position: Option<u64>) -> Result<Option<u64>, Failure> { Err(Failure::Unsupported) }
    fn preferences(&self) -> Result<String, Failure> { Err(Failure::Unsupported) }
    fn save_preferences(&self, _value: &str) -> Result<(), Failure> { Err(Failure::Unsupported) }
}

pub struct NoUsb;
impl Usb for NoUsb {}

pub fn open_file(path: &std::path::Path, mount: Option<u64>) -> Result<std::fs::File, Failure> { super::port().usb.open_file(path, mount) }
pub fn volumes() -> Result<Vec<Volume>, Failure> { super::port().usb.volumes() }
pub fn list_folder(path: &std::path::Path, mount: u64) -> Result<Vec<Entry>, Failure> { super::port().usb.list_folder(path, mount) }
pub fn volume_details(path: &std::path::Path, mount: u64) -> Result<Details, Failure> { super::port().usb.volume_details(path, mount) }
pub fn delete_video(path: &std::path::Path, mount: u64) -> Result<(), Failure> { super::port().usb.delete_video(path, mount) }
pub fn eject_disk(path: &std::path::Path, mount: u64) -> Result<(), String> { super::port().usb.eject_disk(path, mount) }
pub fn folder_stamps(paths: &[std::path::PathBuf], mount: u64) -> Result<Vec<(u64, i64, i64)>, Failure> { super::port().usb.folder_stamps(paths, mount) }
pub fn delete_folder(path: &std::path::Path, mount: u64) -> Result<(), String> { super::port().usb.delete_folder(path, mount) }
pub fn clear_history(base: &str) -> Result<(), Failure> { super::port().usb.clear_history(base) }
pub fn history(prefix: &str) -> Result<std::collections::BTreeMap<String, u64>, Failure> { super::port().usb.history(prefix) }
pub fn resume(key: &str, position: Option<u64>) -> Result<Option<u64>, Failure> { super::port().usb.resume(key, position) }
pub fn preferences() -> Result<String, Failure> { super::port().usb.preferences() }
pub fn save_preferences(value: &str) -> Result<(), Failure> { super::port().usb.save_preferences(value) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_usb_refuses_reads_writes_and_ejection() {
        let usb = NoUsb;
        let path = std::path::Path::new("/tmp/usb/absent");
        assert!(usb.volumes().is_err());
        assert!(usb.open_file(path, Some(1)).is_err());
        assert!(usb.list_folder(path, 1).is_err());
        assert!(usb.volume_details(path, 1).is_err());
        assert!(usb.folder_stamps(&[path.into()], 1).is_err());
        assert!(usb.delete_video(path, 1).is_err());
        assert!(usb.delete_folder(path, 1).is_err());
        assert!(usb.eject_disk(path, 1).is_err());
        assert!(usb.preferences().is_err());
        assert!(usb.save_preferences("ca 0").is_err());
        assert!(usb.history("00").is_err());
        assert!(usb.resume("00", Some(1)).is_err());
        assert!(usb.clear_history("00").is_err());
    }
}
