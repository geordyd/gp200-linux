//! Bounded preferences I/O. Called at bootstrap or by the file worker, never widgets.
use gp200_domain::{AppPreferences, DialogDirectories, DialogDirectoryPurpose};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_PREFERENCES_BYTES: u64 = 64 * 1024;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GP200_STUDIO_CONFIG") {
        return PathBuf::from(path);
    }
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    root.join("gp200-studio/preferences.json")
}

pub fn load_preferences() -> AppPreferences {
    load_preferences_at(&config_path()).0
}

pub(crate) fn load_preferences_report() -> (AppPreferences, Option<String>) {
    load_preferences_at(&config_path())
}

fn load_preferences_at(path: &Path) -> (AppPreferences, Option<String>) {
    let result = (|| {
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_PREFERENCES_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
            return Err(std::io::Error::other("preference file exceeds 64 KiB"));
        }
        let mut preferences: AppPreferences =
            serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        if !preferences.scale.is_finite() || !(0.5..=3.0).contains(&preferences.scale) {
            return Err(std::io::Error::other("invalid appearance preferences"));
        }
        let before = preferences.dialog_directories.len();
        preferences
            .dialog_directories
            .retain(|_, path| valid_directory(path));
        let notice = (before != preferences.dialog_directories.len()).then(||
            "Some remembered dialog folders no longer exist. Those dialogs will use the system default folder.".into());
        Ok((preferences, notice))
    })();
    match result {
        Ok(loaded) => loaded,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (AppPreferences::default(), None)
        }
        Err(error) => (
            AppPreferences::default(),
            Some(format!(
                "Could not read preferences; defaults are in use. The existing file has been kept: {error}"
            )),
        ),
    }
}

fn valid_directory(path: &Path) -> bool {
    path.is_absolute() && path.as_os_str().len() <= 4096 && path.is_dir()
}

pub(crate) fn initial_directory(
    directories: &mut DialogDirectories,
    purpose: DialogDirectoryPurpose,
) -> Option<PathBuf> {
    let directory = directories.get(&purpose)?.clone();
    if valid_directory(&directory) {
        Some(directory)
    } else {
        directories.remove(&purpose);
        None
    }
}

/// Only a completed selection remembers its existing parent. Cancellation does nothing.
pub(crate) fn remember_selection(
    directories: &mut DialogDirectories,
    purpose: DialogDirectoryPurpose,
    selected: Option<&Path>,
) -> bool {
    let Some(directory) = selected
        .and_then(Path::parent)
        .and_then(|parent| parent.canonicalize().ok())
        .filter(|parent| valid_directory(parent))
    else {
        return false;
    };
    if directories.get(&purpose) == Some(&directory) {
        return false;
    }
    directories.insert(purpose, directory);
    true
}

pub(crate) fn save_preferences_at(path: &Path, preferences: &AppPreferences) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(preferences).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
        return Err("Preferences exceed 64 KiB".into());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("Preferences path has no parent directory")?;
    let name = path
        .file_name()
        .ok_or("Preferences path has no file name")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).map_err(|error| error.to_string())?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err("Preference destination is not a regular file; it has been kept".into());
    }
    let stage = parent.join(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Cleanup begins only after create_new proves this worker owns the stage.
    let mut file = options.open(&stage).map_err(|error| error.to_string())?;
    let result = (|| {
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        fs::rename(&stage, path).map_err(|error| error.to_string())?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gp200-preferences-{}-{}",
                std::process::id(),
                NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn legacy_chinese_preferences_load_in_english_and_save_without_losing_other_settings() {
        let temp = Temporary::new();
        let folder = temp.0.join("我的音色");
        fs::create_dir(&folder).unwrap();
        let path = temp.0.join("preferences.json");
        let before = serde_json::json!({
            "language": "zh-CN", "theme": "Light", "scale": 1.5, "compact": true,
            "last_input": "hw:GP200,0,0", "last_output": "hw:GP200,0,0",
            "dialog_directories": {"preset_import": folder}
        });
        let bytes = serde_json::to_vec(&before).unwrap();
        fs::write(&path, &bytes).unwrap();
        let (preferences, notice) = load_preferences_at(&path);
        assert!(notice.is_none());
        assert_eq!(
            preferences.language,
            gp200_domain::LanguagePreference::English
        );
        // Loading migrates the compatibility value in memory. It never rewrites
        // an old file before the normal, private atomic save is requested.
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut expected = before;
        expected["language"] = "en".into();
        assert_eq!(serde_json::to_value(&preferences).unwrap(), expected);
        save_preferences_at(&path, &preferences).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            expected
        );
        assert_eq!(load_preferences_at(&path), (preferences, None));
        assert!(folder.is_dir());
    }
    #[test]
    fn malformed_and_oversized_preferences_keep_the_existing_file() {
        let temp = Temporary::new();
        let path = temp.0.join("prefs.json");
        for bytes in [
            b"invalid JSON".to_vec(),
            vec![b' '; MAX_PREFERENCES_BYTES as usize + 1],
            b"{\"scale\":99}".to_vec(),
        ] {
            fs::write(&path, &bytes).unwrap();
            let (preferences, warning) = load_preferences_at(&path);
            assert_eq!(preferences, AppPreferences::default());
            assert!(warning.is_some());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        assert!(load_preferences_at(&temp.0.join("absent")).1.is_none());
    }
    #[test]
    fn directory_selection_is_separate_cancel_safe_and_revalidated() {
        let temp = Temporary::new();
        let import = temp.0.join("import");
        let export = temp.0.join("export");
        fs::create_dir(&import).unwrap();
        fs::create_dir(&export).unwrap();
        let mut directories = DialogDirectories::new();
        assert!(remember_selection(
            &mut directories,
            DialogDirectoryPurpose::PresetImport,
            Some(&import.join("patch.prst"))
        ));
        assert!(remember_selection(
            &mut directories,
            DialogDirectoryPurpose::PresetExport,
            Some(&export.join("patch.prst"))
        ));
        let before = directories.clone();
        assert!(!remember_selection(
            &mut directories,
            DialogDirectoryPurpose::PresetImport,
            None
        ));
        assert_eq!(directories, before);
        fs::remove_dir(&import).unwrap();
        assert!(
            initial_directory(&mut directories, DialogDirectoryPurpose::PresetImport).is_none()
        );
        assert_eq!(
            initial_directory(&mut directories, DialogDirectoryPurpose::PresetExport),
            Some(export)
        );
    }
    #[test]
    fn atomic_private_roundtrip_and_failed_destination_preserve_data() {
        let temp = Temporary::new();
        let path = temp.0.join("config/preferences.json");
        let mut preferences = AppPreferences {
            theme: gp200_domain::ThemePreference::Dark,
            ..Default::default()
        };
        preferences
            .dialog_directories
            .insert(DialogDirectoryPurpose::CalibrationStimulus, temp.0.clone());
        save_preferences_at(&path, &preferences).unwrap();
        assert_eq!(load_preferences_at(&path).0, preferences);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let obstacle = temp.0.join("obstacle");
        fs::create_dir(&obstacle).unwrap();
        assert!(save_preferences_at(&obstacle, &preferences).is_err());
        assert!(obstacle.is_dir());
        assert_eq!(load_preferences_at(&path).0, preferences);
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }
    #[test]
    fn stale_and_relative_saved_folders_fall_back_without_touching_data() {
        let temp = Temporary::new();
        let path = temp.0.join("preferences.json");
        let mut preferences = AppPreferences::default();
        preferences
            .dialog_directories
            .insert(DialogDirectoryPurpose::NamInput, "relative".into());
        preferences
            .dialog_directories
            .insert(DialogDirectoryPurpose::Firmware, temp.0.join("gone"));
        save_preferences_at(&path, &preferences).unwrap();
        let (loaded, notice) = load_preferences_at(&path);
        assert!(loaded.dialog_directories.is_empty());
        assert!(notice.is_some());
        assert!(fs::read_to_string(&path).unwrap().contains("relative"));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_preference_destination_is_not_followed_or_replaced() {
        let temp = Temporary::new();
        let target = temp.0.join("target");
        fs::write(&target, "keep").unwrap();
        let alias = temp.0.join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        assert!(save_preferences_at(&alias, &AppPreferences::default()).is_err());
        assert!(
            fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    }
}
