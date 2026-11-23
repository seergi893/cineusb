//! An isolated USB library made of empty placeholders for browsing and management.
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{entry_key, favorite_key, pack_progress, tr, Volume, VolumeFacts, FAVORITE, WATCHED};

pub(super) const IDENTITY: &str = "cineusb-demo";
const SHOW: &str = "Series/Turno de noche";
const FILM: &str = "Películas/La última estación.mkv";

pub(super) struct Library {
    pub volume: Volume,
    pub history: BTreeMap<String, u64>,
}

impl Library {
    pub(super) fn create() -> io::Result<Self> {
        static SESSION: AtomicU64 = AtomicU64::new(0);
        let sequence = SESSION.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!("cineusb-demo-{}-{stamp}-{sequence}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&path)?;
        let mut library = Self {
            volume: Volume { path, label: Some(tr("USB de demostración", "USB de demostració").into()),
                mount_id: None, system: false },
            history: BTreeMap::new(),
        };
        library.populate()?;
        Ok(library)
    }

    fn populate(&mut self) -> io::Result<()> {
        for path in [
            FILM, "Películas/Un mapa del viento.mkv", "Películas/Los días azules.mkv",
            "Películas/Colecciones/Ciudades al amanecer/La estación vacía.mkv",
            "Películas/Colecciones/Ciudades al amanecer/Un puente de papel.mkv",
            "Películas/Colecciones/Noches de verano/El faro de las ocho.mkv",
            "Documentales/Voces del bosque.mkv", "Documentales/Océanos/Mar de invierno.mkv",
            "Documentales/Planeta cercano/Temporada 1/Planeta cercano S01E01.mkv",
            "Documentales/Planeta cercano/Temporada 1/Planeta cercano S01E02.mkv",
            "Series/Turno de noche/Turno de noche S01E03.mkv",
            "Series/Turno de noche/Turno de noche S01E04.mkv",
            "Series/Turno de noche/Turno de noche S01E05.mkv",
            "Series/Colección de misterio/Isla de piedra/Temporada 1/01.mkv",
            "Series/Colección de misterio/Isla de piedra/Temporada 1/02.mkv",
            "Series/Colección de misterio/Isla de piedra/Temporada 2/01.mkv",
            "Series/Puertas abiertas/Capítulo 01.mkv",
            "Series/Puertas abiertas/Capítulo 02.mkv",
            "Series/Puertas abiertas/Capítulo 03.mkv",
            "Series/El último tren/Miniserie/El último tren S01E01.mkv",
            "Series/El último tren/Miniserie/El último tren S01E02.mkv",
        ] {
            self.add_video(path)?;
        }
        for collection in ["Horizontes", "Historias del camino", "El otro lado"] {
            for number in 1..=24 {
                self.add_video(&format!("Películas/Catálogo/{collection} {number:02}.mkv"))?;
            }
        }
        for season in 1..=3 {
            for episode in 1..=8 {
                self.add_video(&format!(
                    "Series/Barrio Norte/Temporada {season}/Barrio Norte S{season:02}E{episode:02}.mkv"
                ))?;
            }
        }

        let show = self.volume.path.join(SHOW);
        let key = favorite_key(IDENTITY, &self.volume, &show).expect("demo show is below its volume");
        self.history.insert(key, FAVORITE);
        for episode in 1..=2 {
            let watched = show.join(format!("Turno de noche S01E{episode:02}.mkv"));
            let key = entry_key(IDENTITY, &self.volume, &watched, 0, 0).expect("demo episode is below its volume");
            self.history.insert(key, WATCHED);
        }
        let path = self.volume.path.join(FILM);
        let metadata = path.metadata()?;
        let key = entry_key(IDENTITY, &self.volume, &path, metadata.len(), metadata.mtime())
            .expect("demo film is below its volume");
        self.history.insert(key, pack_progress(1_350_000_000_000, Some(6_000_000_000_000)));
        Ok(())
    }

    fn add_video(&self, name: &str) -> io::Result<()> {
        let path = self.volume.path.join(name);
        fs::create_dir_all(path.parent().expect("demo video has a parent"))?;
        File::create(path)?;
        Ok(())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.volume.path) {
            if error.kind() != io::ErrorKind::NotFound {
                plx_base::eventlog::log(&format!("cineusb: demo cleanup failed: {error}"));
            }
        }
    }
}

pub(super) fn facts() -> VolumeFacts {
    (None, Some(IDENTITY.into()))
}

pub(super) fn delete(root: &Path, path: &Path, directory: bool) -> Result<(), String> {
    let relative = path.strip_prefix(root)
        .map_err(|error| error.to_string())?;
    if relative.as_os_str().is_empty() || !relative.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err("invalid demo path".into());
    }
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    if path == root || !path.starts_with(&root) {
        return Err("path is outside the demo library".into());
    }
    if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) }.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn demo_recommends_episode_three_and_restores_files_on_reentry() {
        let library = Library::create().unwrap();
        let stop = AtomicBool::new(false);
        let dashboard = super::super::home::load_demo(vec![library.volume.clone()], library.history.clone(), &stop).unwrap();
        assert_eq!(dashboard.items[0].episode, Some((1, 3)));
        assert!(dashboard.items[0].favorite);
        assert!(dashboard.items.iter().any(|item| item.path.ends_with(FILM) && item.progress > 0));
        assert!(dashboard.drives[0].videos.len() > 100);
        let root = library.volume.path.clone();
        let episode = dashboard.items[0].path.clone();
        assert!(delete(&root, &root, true).is_err());
        delete(&root, &episode, false).unwrap();
        let updated = super::super::home::load_demo(vec![library.volume.clone()], library.history.clone(), &stop).unwrap();
        assert_eq!(updated.items[0].episode, Some((1, 4)));
        drop(library);
        assert!(!root.exists());

        let fresh = Library::create().unwrap();
        let restored = super::super::home::load_demo(vec![fresh.volume.clone()], fresh.history.clone(), &stop).unwrap();
        assert_eq!(restored.items[0].episode, Some((1, 3)));
    }
}
