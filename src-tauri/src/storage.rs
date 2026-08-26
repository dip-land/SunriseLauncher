use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::error::{AppError, AppResult};
use crate::models::{
    GAME_EXECUTABLE, InstallationSnapshot, InstallerState, MOD_RELATIVE_PATH, Preferences,
};

pub fn app_data_dir(app: &AppHandle) -> AppResult<PathBuf> {
    app.path().app_local_data_dir().map_err(|error| {
        AppError::message(format!(
            "The application data folder is unavailable: {error}"
        ))
    })
}

pub fn app_cache_dir(app: &AppHandle) -> AppResult<PathBuf> {
    app.path().app_cache_dir().map_err(|error| {
        AppError::message(format!(
            "The application cache folder is unavailable: {error}"
        ))
    })
}

pub fn default_install_directory() -> AppResult<String> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::io("Could not locate the launcher executable", error))?;
    let directory = executable
        .parent()
        .ok_or_else(|| AppError::message("The launcher executable has no parent folder."))?;
    Ok(directory.to_string_lossy().into_owned())
}

pub fn is_existing_installation_directory(path: &Path) -> bool {
    path.join(GAME_EXECUTABLE).is_file() || state_path(path).is_file()
}

pub async fn load_preferences(app: &AppHandle) -> AppResult<Preferences> {
    let path = app_data_dir(app)?.join("preferences.json");
    if !path.exists() {
        return Ok(Preferences::default());
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io("Could not read preferences", error))?;
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

pub async fn save_preferences(app: &AppHandle, preferences: &Preferences) -> AppResult<()> {
    let root = app_data_dir(app)?;
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| AppError::io("Could not create the application data folder", error))?;
    let bytes = serde_json::to_vec_pretty(preferences)?;
    write_atomic(&root.join("preferences.json"), &bytes).await
}

pub async fn load_state(install_root: &Path) -> AppResult<Option<InstallerState>> {
    let path = state_path(install_root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io("Could not read the installation state", error))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub async fn save_state(install_root: &Path, state: &InstallerState) -> AppResult<()> {
    let path = state_path(install_root);
    let bytes = serde_json::to_vec_pretty(state)?;
    write_atomic(&path, &bytes).await
}

pub async fn inspect_installation(path: &str) -> AppResult<InstallationSnapshot> {
    if path.trim().is_empty() {
        return Ok(InstallationSnapshot::missing(
            "Choose an installation folder to get started.",
        ));
    }
    let root = PathBuf::from(path.trim());
    let game_found = root.join(GAME_EXECUTABLE).is_file();
    let Some(state) = load_state(&root).await? else {
        return Ok(InstallationSnapshot {
            status: if game_found {
                "unmanaged"
            } else {
                "notInstalled"
            }
            .into(),
            message: if game_found {
                "Destiny 2 was found, but this launcher has not installed Sunrise here."
            } else {
                "No managed Sunrise installation was found in this folder."
            }
            .into(),
            game_found,
            installed_release: None,
            installed_release_digest: None,
            installed_at: None,
            local_file_changed: false,
        });
    };

    let dll = mod_path(&root);
    if !dll.is_file() {
        return Ok(InstallationSnapshot {
            status: "needsRepair".into(),
            message: "The Sunrise DLL is missing. Run Repair.".into(),
            game_found,
            installed_release: Some(state.release_tag),
            installed_release_digest: state.release_asset_digest,
            installed_at: Some(state.installed_at_utc),
            local_file_changed: true,
        });
    }

    let expected = state.installed_dll_sha256.clone();
    let actual = hash_file(&dll).await?;
    let changed = !expected.eq_ignore_ascii_case(&actual);
    Ok(InstallationSnapshot {
        status: if changed { "modified" } else { "installed" }.into(),
        message: if changed {
            "The installed DLL differs from the recorded release. Run Repair or Update."
        } else {
            "Sunrise is installed and ready."
        }
        .into(),
        game_found,
        installed_release: Some(state.release_tag),
        installed_release_digest: state.release_asset_digest,
        installed_at: Some(state.installed_at_utc),
        local_file_changed: changed,
    })
}

pub async fn hash_file(path: &Path) -> AppResult<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .map_err(|error| AppError::io("Could not open a file for verification", error))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| AppError::io("Could not verify a file", error))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|error| {
        AppError::message(format!("File verification stopped unexpectedly: {error}"))
    })?
}

pub fn state_path(install_root: &Path) -> PathBuf {
    install_root.join(".sunrise").join("install-state.json")
}

pub fn mod_path(install_root: &Path) -> PathBuf {
    MOD_RELATIVE_PATH
        .iter()
        .fold(install_root.to_path_buf(), |path, part| path.join(part))
}

pub fn new_state(
    release_tag: String,
    release_asset: String,
    release_asset_digest: Option<String>,
    installed_dll_sha256: String,
) -> InstallerState {
    InstallerState {
        schema_version: 1,
        app_id: crate::models::STEAM_APP_ID,
        release_tag,
        release_asset,
        release_asset_digest,
        installed_dll_sha256,
        installed_at_utc: chrono::Utc::now(),
        manifests: BTreeMap::from(crate::models::DEPOTS),
    }
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::message("The data file has no parent folder."))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| AppError::io("Could not create a data folder", error))?;
    let temporary = parent.join(format!(
        ".{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(|error| AppError::io("Could not write local installer data", error))?;
    if path.exists() {
        tokio::fs::remove_file(path)
            .await
            .map_err(|error| AppError::io("Could not replace local installer data", error))?;
    }
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| AppError::io("Could not finish writing local installer data", error))?;
    Ok(())
}
