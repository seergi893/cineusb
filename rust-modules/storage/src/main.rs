#[cfg(all(target_os = "linux", target_arch = "arm"))]
#[path = "../../platform/src/storage_service/auxv.rs"]
mod auxv;
#[path = "../../platform/src/storage_service/backend.rs"]
#[cfg_attr(not(all(target_os = "linux", target_arch = "arm")), allow(dead_code))]
mod backend;
#[path = "../../platform/src/storage_service/keymanager.rs"]
#[cfg_attr(not(all(target_os = "linux", target_arch = "arm")), allow(dead_code))]
mod keymanager;
#[path = "../../platform/src/storage_service/runtime.rs"]
#[cfg_attr(not(all(target_os = "linux", target_arch = "arm")), allow(dead_code))]
mod runtime;
#[path = "../../platform/src/storage/state.rs"]
#[allow(dead_code)] // Shared engine also exposes adapter APIs unused by this executable.
mod state;
#[path = "../../platform/src/storage_service/wire.rs"]
#[cfg_attr(not(all(target_os = "linux", target_arch = "arm")), allow(dead_code))]
mod wire;

#[cfg(all(target_os = "linux", target_arch = "arm"))]
#[path = "../../platform/src/storage_service/service.rs"]
mod service;

#[cfg(all(target_os = "linux", any(target_arch = "arm", test)))]
#[path = "../../platform/src/webos/usb_fd.rs"]
mod usb_fd;
#[cfg(all(target_os = "linux", any(target_arch = "arm", test)))]
mod usb_open;
#[cfg(all(target_os = "linux", any(target_arch = "arm", test)))]
mod usb_preferences;

fn main() {
    #[cfg(all(target_os = "linux", any(target_arch = "arm", test)))]
    {
        let args: Vec<_> = std::env::args().collect();
        if let [_, mode, parameters @ ..] = args.as_slice() {
            let result = match (mode.as_str(), parameters) {
                ("--cineusb-open", [pid, slot, id, path]) => Some(usb_open::run(pid, slot, id, path)),
                ("--cineusb-volumes", []) => Some(usb_open::list_volumes()),
                ("--cineusb-list", [id, path]) => Some(usb_open::list_folder(id, path)),
                ("--cineusb-delete", [id, path]) => Some(usb_open::delete(id, path)),
                ("--cineusb-delete-folder", [id, path]) => Some(usb_open::delete_folder(id, path)),
                ("--cineusb-eject", [id, path]) => Some(usb_open::eject(id, path)),
                ("--cineusb-preferences", []) => Some(usb_preferences::read()),
                ("--cineusb-preferences", [language, forced, audio]) => Some(usb_preferences::write(language, forced, audio)),
                ("--cineusb-open" | "--cineusb-volumes" | "--cineusb-list" | "--cineusb-delete" | "--cineusb-eject", _) => {
                    Some(Err(std::io::Error::other("invalid CineUSB USB request")))
                }
                _ => None,
            };
            if let Some(result) = result {
                match result {
                    Ok(()) => return,
                    Err(error) => eprintln!("CineUSB USB request failed: {error}"),
                }
                std::process::exit(1);
            }
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    if service::run().is_err() {
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "arm")))]
    {
        eprintln!("plxnative-storage requires the webOS Linux service runtime");
        std::process::exit(1);
    }
}
