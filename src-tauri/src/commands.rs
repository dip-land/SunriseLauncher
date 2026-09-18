use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, State, ipc::Channel};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::github::GitHubClient;
use crate::installer;
use crate::models::{
    AppSnapshot, InstallationSnapshot, OperationEvent, OperationRequest, OperationResult,
    Preferences, current_platform, resolve_language,
};
use crate::storage;

#[derive(Default)]
pub struct OperationState {
    active: AtomicBool,
    cancel: Mutex<Option<CancellationToken>>,
    terminal_input: Arc<AsyncMutex<Option<ChildStdin>>>,
}

impl OperationState {
    pub fn cancel_active(&self) -> bool {
        let guard = self.cancel.lock().expect("operation cancel mutex poisoned");
        if let Some(cancel) = guard.as_ref() {
            cancel.cancel();
            true
        } else {
            false
        }
    }
}

impl Drop for OperationState {
    fn drop(&mut self) {
        if let Ok(guard) = self.cancel.lock()
            && let Some(cancel) = guard.as_ref()
        {
            cancel.cancel();
        }
    }
}

#[tauri::command]
pub async fn get_app_snapshot(app: AppHandle) -> AppResult<AppSnapshot> {
    let mut preferences = storage::load_preferences(&app).await?;
    preferences.steam_language = resolve_language(&preferences.steam_language)
        .steam_language
        .into();
    let launcher_directory = storage::default_install_directory()?;
    let launcher_is_in_installation =
        storage::is_existing_installation_directory(Path::new(&launcher_directory));
    let detected_directory =
        if launcher_is_in_installation || preferences.install_directory.trim().is_empty() {
            Some(launcher_directory)
        } else {
            None
        };
    if let Some(detected_directory) = detected_directory
        && preferences.install_directory != detected_directory
    {
        preferences.install_directory = detected_directory;
        storage::save_preferences(&app, &preferences).await?;
    }
    let installation = storage::inspect_installation(&preferences.install_directory).await?;
    // Repair and Install use the selected language, so it starts as the installed one.
    if let Some(installed) = installation.steam_language.as_ref()
        && preferences.steam_language != *installed
    {
        preferences.steam_language = installed.clone();
        storage::save_preferences(&app, &preferences).await?;
    }
    let release_result = async {
        GitHubClient::new()?
            .latest_release("stanuwu", "Sunrise", "steam_api64.dll")
            .await
    }
    .await;
    let (latest_release, release_error) = match release_result {
        Ok(release) => (Some(release), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let update_available = latest_release
        .as_ref()
        .is_some_and(|release| installation.update_available(release));
    Ok(AppSnapshot {
        platform: current_platform(),
        preferences,
        installation,
        latest_release,
        update_available,
        release_error,
    })
}

#[tauri::command]
pub async fn inspect_installation(install_directory: String) -> AppResult<InstallationSnapshot> {
    storage::inspect_installation(&install_directory).await
}

#[tauri::command]
pub async fn save_preferences(app: AppHandle, mut preferences: Preferences) -> AppResult<()> {
    preferences.steam_language = resolve_language(&preferences.steam_language)
        .steam_language
        .into();
    storage::save_preferences(&app, &preferences).await
}

#[tauri::command]
pub async fn run_operation(
    app: AppHandle,
    state: State<'_, OperationState>,
    request: OperationRequest,
    on_event: Channel<OperationEvent>,
) -> AppResult<OperationResult> {
    if state
        .active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(AppError::message(
            "Another installer operation is already running.",
        ));
    }

    let cancel = CancellationToken::new();
    *state
        .cancel
        .lock()
        .expect("operation cancel mutex poisoned") = Some(cancel.clone());
    let preferences = Preferences {
        install_directory: request.install_directory.clone(),
        steam_username: request.steam_username.clone(),
        steam_language: resolve_language(&request.steam_language)
            .steam_language
            .into(),
        auth_method: request.auth_method,
        launch_command: request.launch_command.clone(),
    };
    let result = async {
        storage::save_preferences(&app, &preferences).await?;
        installer::run(
            &app,
            request,
            on_event,
            cancel,
            state.terminal_input.clone(),
        )
        .await
    }
    .await;

    *state.terminal_input.lock().await = None;
    *state
        .cancel
        .lock()
        .expect("operation cancel mutex poisoned") = None;
    state.active.store(false, Ordering::Release);
    result
}

#[tauri::command]
pub async fn send_terminal_input(
    state: State<'_, OperationState>,
    input: String,
) -> AppResult<bool> {
    if input.len() > 512
        || input
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(AppError::message("The console response is not valid."));
    }
    let mut guard = state.terminal_input.lock().await;
    let Some(stdin) = guard.as_mut() else {
        return Ok(false);
    };
    stdin
        .write_all(format!("{input}\n").as_bytes())
        .await
        .map_err(|error| AppError::io("Could not send input to DepotDownloader", error))?;
    stdin
        .flush()
        .await
        .map_err(|error| AppError::io("Could not send input to DepotDownloader", error))?;
    Ok(true)
}

#[tauri::command]
pub fn cancel_operation(state: State<'_, OperationState>) -> bool {
    state.cancel_active()
}

#[tauri::command]
pub fn launch_game(install_directory: String, launch_command: Option<String>) -> AppResult<()> {
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "linux")]
        {
            return launch_linux(&PathBuf::from(install_directory.trim()), launch_command);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (install_directory, launch_command);
        return Err(AppError::message(
            "Launching is currently supported on Windows and Linux.",
        ));
    }
    #[cfg(windows)]
    {
        let root = PathBuf::from(install_directory.trim());
        let _ = launch_command;
        launch_windows(&root)
    }
}

#[cfg(windows)]
fn launch_windows(root: &Path) -> AppResult<()> {
    let executable = root.join(crate::models::GAME_EXECUTABLE);
    if !executable.is_file() {
        return Err(AppError::message(
            "destiny2.exe was not found in the installation folder.",
        ));
    }
    std::process::Command::new(executable)
        .current_dir(root)
        .spawn()
        .map_err(|error| AppError::io("Destiny 2 could not be launched", error))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn launch_linux(root: &Path, launch_command: Option<String>) -> AppResult<()> {
    let command = launch_command
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| {
            AppError::message("Enter a Linux launch command in Settings before launching.")
        })?;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .spawn()
        .map_err(|error| AppError::io("The game launch command could not be started", error))?;
    Ok(())
}
