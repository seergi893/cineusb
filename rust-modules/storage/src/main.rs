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

fn main() {
    #[cfg(all(target_os = "linux", any(target_arch = "arm", test)))]
    {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|arg| arg == "--cineusb-open") {
            match args.as_slice() {
                [_, _, pid, slot, path] => match usb_open::run(pid, slot, path) {
                    Ok(()) => return,
                    Err(error) => eprintln!("CineUSB USB open failed: {error}"),
                },
                _ => eprintln!("Invalid CineUSB USB open request"),
            }
            std::process::exit(1);
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
