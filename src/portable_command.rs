// Adapted from Wangnov/Codex-App-Manager. See LICENSE and NOTICE.
use std::{io, path::Path, process::Command};

/// Use the payload's CLI without changing the user's or the Manager's environment.
/// A nonempty explicit user override retains its normal upstream semantics.
pub fn configure(command: &mut Command, app_exe: &Path, required: bool) -> io::Result<()> {
    configure_with_override(command, app_exe, required, None)
}

fn configure_with_override(
    command: &mut Command,
    app_exe: &Path,
    required: bool,
    cli_override: Option<std::ffi::OsString>,
) -> io::Result<()> {
    let root = app_exe
        .parent()
        .ok_or_else(|| io::Error::other("missing app directory"))?;
    command.current_dir(root);
    command.env_remove("CODEX_WINDOWS_REGISTERED_CORE");
    let overridden = cli_override.is_some_and(|value| !value.to_string_lossy().trim().is_empty());
    if !overridden {
        let cli = [
            root.join("resources/codex.exe"),
            root.join("resources/bin/codex.exe"),
        ]
        .into_iter()
        .find(|path| path.is_file());
        if let Some(cli) = cli {
            command.env("CODEX_CLI_PATH", cli);
        } else if required {
            return Err(io::Error::new(io::ErrorKind::NotFound,
                "Portable Codex is missing its bundled CLI (resources/codex.exe). Restage the official MSIX."));
        }
    }
    Ok(())
}
