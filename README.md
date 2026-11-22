# Codex Portable Lite

A command-line installer and updater for portable ChatGPT / Codex on Windows x64 and ARM64. **GPT-6.2 Sol** wrote the tool and adapted the upstream code. See [NOTICE](NOTICE) for attribution and [LICENSE](LICENSE) for the MIT license.

## Usage

Building from source requires Rust and the MSVC build tools.

```powershell
cargo build --release --locked
$cli = '.\target\release\codex-portable-lite.exe'
& $cli update   # Download, verify, check startup, and activate
& $cli launch   # Open the active portable installation
```

Double-click `Update.cmd` to install or update. The default install directory is `%LOCALAPPDATA%\CodexPortableLite`. For a custom location, start with a dedicated empty directory and use the same path for subsequent commands.

```powershell
.\Update.cmd 'D:\Apps\CodexPortable'
& $cli status   --install-dir 'D:\Apps\CodexPortable'
& $cli check    --install-dir 'D:\Apps\CodexPortable'
& $cli stage    --install-dir 'D:\Apps\CodexPortable'
& $cli apply    --install-dir 'D:\Apps\CodexPortable'
& $cli health   --install-dir 'D:\Apps\CodexPortable'
& $cli rollback --install-dir 'D:\Apps\CodexPortable'
& $cli launch   --install-dir 'D:\Apps\CodexPortable'
```

`Update.cmd` also accepts the `CODEX_PORTABLE_LITE_DIR` environment variable. `stage` downloads and verifies a package for later use. `apply` can activate a staged package offline. `update` runs both steps. If the old version is still open, quit it before the next launch so Electron starts the newly activated version.

## Download and activation

- The tool selects the matching MSIX from `Wangnov/codex-app-mirror` on GitHub Releases. It checks the asset size and SHA-256, verifies the Windows Authenticode signature and OpenAI Store publisher, and checks the package manifest. `apply` repeats these checks before extraction.
- Extraction preserves the MSIX logical filenames and `app/` layout, including the `@scope` paths required by Computer Use. The launcher points the child process at the bundled official `codex.exe`.
- The startup check opens the candidate with temporary Electron and Codex profiles and waits for a stable main window. The tool activates the candidate only after the check passes. `rollback` switches to the previous successful version. Account sign-in and Windows sandbox enforcement require separate testing.
- Installed versions live under `v/` in the install directory. Older version folders are kept. After quitting a version, you can remove its folder if it is neither the active nor the rollback version. The staged MSIX also uses disk space.

The tool runs as the current user and extracts a portable installation. It downloads OpenAI's official binaries separately, and those binaries remain subject to OpenAI's terms.

Downloads use the `curl.exe` bundled with Windows. To reach GitHub through a proxy, set `HTTPS_PROXY` in the terminal you use to run the CLI or `Update.cmd`.
