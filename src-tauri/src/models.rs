use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const STEAM_APP_ID: u32 = 1_085_660;
pub const GAME_EXECUTABLE: &str = "destiny2.exe";
pub const MOD_RELATIVE_PATH: [&str; 3] = ["bin", "x64", "steam_api64.dll"];
pub const FRESH_INSTALL_BYTES: u64 = 110 * 1024 * 1024 * 1024;
pub const REPAIR_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub const UPDATE_BYTES: u64 = 256 * 1024 * 1024;
pub const DEPOTS: [(u32, u64); 2] = [
    (1_085_661, 7_180_122_903_232_116_872),
    (1_085_662, 2_210_332_166_360_342_287),
];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub install_directory: String,
    pub steam_username: String,
    #[serde(default)]
    pub auth_method: AuthMethod,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AuthMethod {
    #[default]
    Qr,
    TwoFactor,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformSupport {
    pub os: String,
    pub arch: String,
    pub level: String,
    pub can_launch: bool,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallerState {
    #[serde(default = "schema_version")]
    pub schema_version: u8,
    #[serde(default = "steam_app_id")]
    pub app_id: u32,
    pub release_tag: String,
    pub release_asset: String,
    pub release_asset_digest: Option<String>,
    pub installed_dll_sha256: String,
    pub installed_at_utc: DateTime<Utc>,
    pub manifests: BTreeMap<u32, u64>,
}

fn schema_version() -> u8 {
    1
}

fn steam_app_id() -> u32 {
    STEAM_APP_ID
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationSnapshot {
    pub status: String,
    pub message: String,
    pub game_found: bool,
    pub installed_release: Option<String>,
    pub installed_release_digest: Option<String>,
    pub installed_at: Option<DateTime<Utc>>,
    pub local_file_changed: bool,
}

impl InstallationSnapshot {
    pub fn missing(message: impl Into<String>) -> Self {
        Self {
            status: "notInstalled".into(),
            message: message.into(),
            game_found: false,
            installed_release: None,
            installed_release_digest: None,
            installed_at: None,
            local_file_changed: false,
        }
    }

    pub fn update_available(&self, latest: &ReleaseInfo) -> bool {
        let Some(installed_tag) = self.installed_release.as_deref() else {
            return false;
        };
        if !installed_tag.eq_ignore_ascii_case(&latest.tag) {
            return true;
        }

        let Some(installed_digest) = self
            .installed_release_digest
            .as_deref()
            .filter(|digest| !digest.trim().is_empty())
        else {
            return false;
        };
        let Some(latest_digest) = latest
            .digest
            .as_deref()
            .filter(|digest| !digest.trim().is_empty())
        else {
            return false;
        };

        !installed_digest.eq_ignore_ascii_case(latest_digest)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub platform: PlatformSupport,
    pub preferences: Preferences,
    pub installation: InstallationSnapshot,
    pub latest_release: Option<ReleaseInfo>,
    pub update_available: bool,
    pub release_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationKind {
    Install,
    Repair,
    Update,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRequest {
    pub kind: OperationKind,
    pub install_directory: String,
    #[serde(default)]
    pub steam_username: String,
    #[serde(default)]
    pub auth_method: AuthMethod,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub changed: bool,
    pub release_tag: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "event")]
pub enum OperationEvent {
    Progress {
        stage: String,
        message: String,
        percent: f32,
    },
    Terminal {
        stream: String,
        text: String,
    },
    QrCode {
        rows: Vec<String>,
    },
    AuthPrompt {
        kind: String,
        message: String,
    },
    AuthComplete,
    Notice {
        message: String,
    },
}

pub fn current_platform() -> PlatformSupport {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    match std::env::consts::OS {
        "windows" => PlatformSupport {
            os,
            arch,
            level: "supported".into(),
            can_launch: true,
            summary: "Native install, repair, update, and launch support.".into(),
        },
        "linux" => PlatformSupport {
            os,
            arch,
            level: "experimental".into(),
            can_launch: false,
            summary: "Native installation is supported; launching through Proton is a follow-up integration.".into(),
        },
        "macos" => PlatformSupport {
            os,
            arch,
            level: "installOnly".into(),
            can_launch: false,
            summary: "The Windows game files can be managed, but Sunrise cannot currently run on macOS.".into(),
        },
        _ => PlatformSupport {
            os,
            arch,
            level: "unsupported".into(),
            can_launch: false,
            summary: "This platform does not have a matching DepotDownloader build.".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{InstallationSnapshot, InstallerState, ReleaseInfo};

    fn installed_snapshot(digest: Option<&str>) -> InstallationSnapshot {
        InstallationSnapshot {
            status: "installed".into(),
            message: "Sunrise is installed and ready.".into(),
            game_found: true,
            installed_release: Some("v1.2.3".into()),
            installed_release_digest: digest.map(str::to_owned),
            installed_at: None,
            local_file_changed: false,
        }
    }

    fn latest_release(tag: &str, digest: Option<&str>) -> ReleaseInfo {
        ReleaseInfo {
            tag: tag.into(),
            asset_name: "steam_api64.dll".into(),
            download_url: "https://example.invalid/steam_api64.dll".into(),
            size: 1,
            digest: digest.map(str::to_owned),
        }
    }

    #[test]
    fn update_detection_matches_the_legacy_tag_and_digest_rules() {
        let installed = installed_snapshot(Some("sha256:aabb"));
        assert!(!installed.update_available(&latest_release("V1.2.3", Some("SHA256:AABB"))));
        assert!(installed.update_available(&latest_release("v1.2.4", Some("sha256:aabb"))));
        assert!(installed.update_available(&latest_release("v1.2.3", Some("sha256:ccdd"))));
        assert!(!installed.update_available(&latest_release("v1.2.3", None)));
    }

    #[test]
    fn reads_install_state_written_by_the_legacy_launcher() {
        let state: InstallerState = serde_json::from_str(
            r#"{
                "schemaVersion": 1,
                "appId": 1085660,
                "releaseTag": "v1.2.3",
                "releaseAsset": "steam_api64.dll",
                "releaseAssetDigest": "sha256:aabb",
                "installedDllSha256": "ccdd",
                "installedAtUtc": "2026-08-26T12:00:00+00:00",
                "manifests": { "1085661": 7180122903232116872 }
            }"#,
        )
        .expect("legacy state should remain compatible");

        assert_eq!(state.release_tag, "v1.2.3");
        assert_eq!(state.release_asset_digest.as_deref(), Some("sha256:aabb"));
        assert_eq!(
            state.manifests.get(&1_085_661),
            Some(&7_180_122_903_232_116_872)
        );
    }
}
