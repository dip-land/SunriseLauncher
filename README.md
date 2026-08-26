# Project Sunrise Launcher

A Tauri 2 installer and launcher for [Project Sunrise](https://github.com/stanuwu/Sunrise), the Destiny 2 offline exploration mod.

This repository is an early functional rewrite of [SunriseInstaller](https://github.com/stanuwu/SunriseInstaller). The interface, filesystem access, downloads, hashing, and child-process orchestration are split across a TypeScript frontend and a Rust backend. Steam credentials are handled by DepotDownloader and are never persisted by the launcher.

## Platform support

| Platform | Install / repair / update | Launch |
| --- | --- | --- |
| Windows x64 / ARM64 | Supported | Supported |
| Linux x64 / ARM64 | Experimental | Proton integration pending |
| macOS x64 / ARM64 | File management only | Not supported by the game/mod |

"Cross-platform" applies to the installer. Sunrise remains a Windows DLL for a Windows build of Destiny 2.

## Current installer flow

1. Validate the target folder, write access, and free space.
2. Download the matching native DepotDownloader release for the host OS/architecture.
3. Stream DepotDownloader output into the in-app console for QR/Steam Guard authentication.
4. Download the two pinned, owned Steam depot manifests.
5. Download `steam_api64.dll` from the latest Sunrise GitHub release.
6. Verify GitHub's SHA-256 digest, preserve rollback/original copies, and install the DLL.
7. Save a compatible `.sunrise/install-state.json` for update and integrity checks.

The pinned manifests are the same ones used by the current installer:

- Depot `1085661`, manifest `7180122903232116872`
- Depot `1085662`, manifest `2210332166360342287`

## Development

Requirements: Node.js, Rust, and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your operating system.

```powershell
npm install
npm run tauri dev
```

The frontend can also be previewed without native commands. It automatically uses mock data under `npm run dev`; packaged Tauri builds always call the Rust backend.

### Checks

```powershell
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml
```

### Package locally

```powershell
npm run tauri build
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for trust boundaries, installer behavior, and planned Proton support.

## Distribution notes

- Production Windows and macOS downloads should be code-signed before broad distribution.
- The GitHub workflow builds native installers from a `launcher-v*` tag and leaves the release as a draft for review.
- Test the complete Steam authentication/download flow on each target OS before marking a build stable; automated tests intentionally do not download the ~110 GiB game payload.

## License and credits

GPL-2.0-only. DepotDownloader is downloaded on demand as a separate GPL-2.0 program and is not linked into this application. Logo artwork is credited to Solus, matching the existing Sunrise installer attribution.
