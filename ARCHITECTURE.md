# Architecture

## Why Tauri

Tauri is a good fit because this application needs a rich cross-platform interface, but its important work is native: large downloads, child-process I/O, filesystem validation, hashing, safe replacement, and game launch. The webview is treated as a presentation layer. It can invoke only explicitly registered commands; it cannot directly read or mutate arbitrary game files.

## Boundaries

```text
TypeScript UI
  │ typed Tauri commands + ordered progress channel
  ▼
Rust installer core
  ├─ GitHub API/release downloads
  ├─ SHA-256 verification
  ├─ preferences + install-state storage
  ├─ path/free-space/write preflight
  └─ DepotDownloader process supervision
       └─ Steam authentication and owned depot downloads
```

- The frontend owns rendering and short-lived form state.
- Rust owns all network, process, and game-directory access.
- DepotDownloader owns Steam authentication. The launcher stores the account name, but not a password, Steam Guard code, or QR session.
- The Sunrise release digest is verified when GitHub publishes one. DepotDownloader is also digest-verified when its release supplies a digest.
- ZIP entries are restricted to enclosed paths to prevent path traversal.

## Operations

### Install

Requires 110 GiB free for a fresh install, accepts existing folders so DepotDownloader can scan and reuse their files, downloads both pinned depots, preserves the depot-provided `steam_api64.dll`, installs Sunrise, and writes state.

### Repair

Requires an existing game and 5 GiB free, runs DepotDownloader with `-validate`, removes `bin/x64/Sunrise`, reinstalls the latest release, and refreshes state.

### Update

Requires an existing game and 256 MiB free. It compares the release tag, optional release digest, and installed DLL hash. If anything differs, it replaces only the Sunrise payload.

### Cancellation and recovery

Only one operation can run at a time. Cancellation terminates the active DepotDownloader child. DLL replacement stages a new file and keeps `.sunrise/rollback/steam_api64.dll`; a failed hash check restores that copy.

## Platform model

DepotDownloader publishes native Windows, Linux, and macOS binaries for x64 and ARM64. It is therefore possible to install the required Windows depot files from all three desktop operating systems. Running the result is separate:

- Windows launches `destiny2.exe` directly.
- Linux needs an explicit Proton/Wine prefix and compatibility-tool selection. That must be user-configurable rather than guessed.
- macOS has no supported Destiny 2/Sunrise runtime path, so the launcher is intentionally install/manage-only there.

## Next production milestones

1. Add Linux Steam library discovery plus a user-selected Proton version/prefix and launch command.
2. Add signed launcher self-updates and release-channel metadata.
3. Add structured local logs with a redaction policy and an export-for-support action.
4. Add integration tests around mocked GitHub responses, interrupted downloads, malicious ZIPs, and rollback failures.
5. Run full install, repair, resume, cancellation, and Steam Guard tests on clean Windows/Linux/macOS machines.
