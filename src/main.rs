mod archive;
mod extract;
mod health;
mod package;
mod portable_command;

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, thiserror::Error)]
enum EngineError {
    #[error("MSIX extraction: {0}")]
    Msix(String),
    #[error("MSIX I/O: {0}")]
    Io(String),
}

const MARKER: &str = ".codex-portable-lite";
const MARKER_TEXT: &str = "codex-portable-lite v1\n";
const GENERATION_FILE: &str = "codex-lite-package.json";
const USAGE: &str = "Usage: codex-portable-lite <status|check|stage|apply|update|health|rollback|launch> [--install-dir PATH]\n\
    status    Show the active, previous and staged versions without using the network\n\
    check     Check the mirror for an update\n\
    stage     Download and verify the latest package without activating it\n\
    apply     Activate a staged package after a startup check\n\
    update    Check, stage and apply the latest package\n\
    health    Check the active app in a disposable profile\n\
    rollback  Restore the previous active version\n\
    launch    Start the active portable app\n\
    Default install directory: %LOCALAPPDATA%\\CodexPortableLite";

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Active {
    current: Option<String>,
    previous: Option<String>,
}

impl Active {
    fn read(root: &Path) -> Result<Self> {
        let state = read_json::<Self>(&root.join("active.json"))?.unwrap_or_default();
        for version in state.current.iter().chain(state.previous.iter()) {
            if package::version_parts(version).is_none() {
                return Err("Invalid installed version pointer".into());
            }
        }
        if state.previous.is_some() && (state.current.is_none() || state.current == state.previous)
        {
            return Err("Invalid previous version pointer".into());
        }
        Ok(state)
    }
}

fn system_program(relative: &str) -> Result<PathBuf> {
    let windows = env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
    Ok(PathBuf::from(windows).join("System32").join(relative))
}

fn plain(path: &Path, directory: bool) -> Result<()> {
    let md = fs::symlink_metadata(path)?;
    let reparse_point = md.file_attributes() & 0x400 != 0;
    if reparse_point || (directory && !md.is_dir()) || (!directory && !md.is_file()) {
        return Err(format!(
            "Expected a regular {} at {}",
            if directory { "directory" } else { "file" },
            path.display()
        )
        .into());
    }
    Ok(())
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !exists(path)? {
        return Ok(None);
    }
    plain(path, false)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > 65536 {
        return Err(format!("Metadata is too large: {}", path.display()).into());
    }
    Ok(Some(serde_json::from_reader(BufReader::new(file))?))
}

pub(crate) fn atomic_json<T: Serialize>(directory: &Path, name: &str, value: &T) -> Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(output.as_file_mut(), value)?;
    output.as_file_mut().write_all(b"\n")?;
    output.as_file().sync_all()?;
    output.persist(directory.join(name))?;
    Ok(())
}

fn root_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let input = match path {
        Some(path) => path,
        None => PathBuf::from(env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
            .join("CodexPortableLite"),
    };
    if input.as_os_str().is_empty() {
        return Err("Install directory cannot be empty".into());
    }
    let root = std::path::absolute(input)?;
    if !matches!(root.components().next(), Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_)))
        || root.file_name().is_none()
        || root
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case("WindowsApps"))
    {
        return Err("Choose a dedicated directory on a local drive outside WindowsApps".into());
    }
    for path in root.ancestors() {
        if exists(path)? {
            plain(path, true)?;
        }
    }
    Ok(root)
}

fn open_root(root: &Path, create: bool) -> Result<Option<File>> {
    if !exists(root)? {
        if !create {
            return Ok(None);
        }
        fs::create_dir_all(root)?;
    }
    plain(root, true)?;
    let marker = root.join(MARKER);
    if !exists(&marker)? {
        if fs::read_dir(root)?.next().transpose()?.is_some() {
            return Err("Install directory is not empty and is not managed by this tool. Choose an empty directory.".into());
        }
        if !create {
            return Ok(None);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        file.write_all(MARKER_TEXT.as_bytes())?;
        file.sync_all()?;
    }
    plain(&marker, false)?;
    if fs::read_to_string(marker)? != MARKER_TEXT {
        return Err("Unsupported or invalid installation marker".into());
    }
    let lock_path = root.join(".lock");
    if exists(&lock_path)? {
        plain(&lock_path, false)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock().map_err(|error| {
        format!("Cannot lock this installation. Another operation may be running: {error}")
    })?;
    Ok(Some(lock))
}

fn versions(root: &Path) -> Result<PathBuf> {
    let path = root.join("v");
    if !exists(&path)? {
        fs::create_dir(&path)?;
    }
    plain(&path, true)?;
    Ok(path)
}

fn installed_exe(root: &Path, version: &str) -> Result<PathBuf> {
    if package::version_parts(version).is_none() {
        return Err("Invalid installed version".into());
    }
    let version_root = versions(root)?.join(version);
    plain(&version_root, true)?;
    let package = read_json::<package::Package>(&version_root.join(GENERATION_FILE))?
        .ok_or("Installed version marker is missing")?;
    package.validate()?;
    if package.version != version {
        return Err("Installed version marker does not match the active version".into());
    }
    let manifest = version_root.join("AppxManifest.xml");
    plain(&manifest, false)?;
    let exe = version_root.join(package::validate_manifest(
        &fs::read_to_string(manifest)?,
        &package,
    )?);
    plain(exe.parent().ok_or("Invalid application path")?, true)?;
    plain(&exe, false)?;
    Ok(exe)
}

fn apply(root: &Path, active: &Active) -> Result<()> {
    let (package, path) = package::staged(root)?;
    if !newer(&package.version, active.current.as_deref()) {
        println!(
            "Staged {} is already active or older. Active version will stay unchanged.",
            package.version
        );
        return Ok(());
    }
    let _verified_file = package::verify(&path, &package)?;
    let versions = versions(root)?;
    let next = versions.join(&package.version);
    if exists(&next)? {
        plain(&next, true)?;
        let installed = read_json::<package::Package>(&next.join(GENERATION_FILE))?;
        if installed.as_ref() != Some(&package) {
            return Err(
                "The version directory already exists and does not match the verified package"
                    .into(),
            );
        }
    } else {
        eprintln!("Extracting verified MSIX {}…", package.version);
        let temp = tempfile::Builder::new()
            .prefix("extract-")
            .tempdir_in(&versions)?;
        let manifest = extract::extract_msix(&path, temp.path())?;
        package::validate_manifest(&manifest, &package)?;
        plain(&temp.path().join("AppxBlockMap.xml"), false)?;
        atomic_json(temp.path(), GENERATION_FILE, &package)?;
        fs::rename(temp.path(), &next)?;
    }
    let exe = installed_exe(root, &package.version)?;
    eprintln!("Checking application startup in a temporary profile…");
    activate(root, active, &package.version, || health::check(&exe, root))?;
    println!("Activated {}. Startup check passed.", package.version);
    Ok(())
}

fn activate(
    root: &Path,
    active: &Active,
    version: &str,
    health: impl FnOnce() -> Result<()>,
) -> Result<()> {
    health().map_err(|error| {
        format!("Health check failed; the active version was left untouched: {error}")
    })?;
    atomic_json(
        root,
        "active.json",
        &Active {
            current: Some(version.to_string()),
            previous: active.current.clone(),
        },
    )
}

fn newer(candidate: &str, installed: Option<&str>) -> bool {
    package::version_parts(candidate) > installed.and_then(package::version_parts)
}

fn rollback(root: &Path, active: &Active) -> Result<()> {
    let previous = active
        .previous
        .as_deref()
        .ok_or("There is no previous installed version")?;
    installed_exe(root, previous)?;
    atomic_json(
        root,
        "active.json",
        &Active {
            current: active.previous.clone(),
            previous: active.current.clone(),
        },
    )?;
    println!("Restored {previous}. Use launch after closing the old window.");
    Ok(())
}

fn status(root: &Path, active: &Active) -> Result<()> {
    let staged = read_json::<package::Package>(&root.join("staged.json"))?;
    if let Some(package) = &staged {
        package.validate()?;
    }
    println!("Install directory: {}", root.display());
    println!("Active:   {}", active.current.as_deref().unwrap_or("none"));
    println!("Previous: {}", active.previous.as_deref().unwrap_or("none"));
    println!(
        "Staged:   {}",
        staged
            .as_ref()
            .map(|item| item.version.as_str())
            .unwrap_or("none")
    );
    Ok(())
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let Some(command) = args.next() else {
        println!("{USAGE}");
        return Ok(());
    };
    let command = command.to_str().ok_or("Invalid command")?;
    if matches!(command, "--help" | "-h" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    if command == "--version" {
        println!("codex-portable-lite {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !matches!(
        command,
        "status" | "check" | "stage" | "apply" | "update" | "health" | "rollback" | "launch"
    ) {
        return Err(format!("Unknown command '{command}'. Run --help.").into());
    }
    let directory = match args.next() {
        Some(flag) if flag == "--install-dir" => Some(PathBuf::from(
            args.next().ok_or("--install-dir requires a path")?,
        )),
        Some(_) => return Err("Expected --install-dir PATH".into()),
        None => None,
    };
    if args.next().is_some() {
        return Err("Unexpected additional arguments".into());
    }
    let root = root_path(directory)?;
    let lock = open_root(&root, matches!(command, "stage" | "update"))?;
    if lock.is_none() && !matches!(command, "status" | "check") {
        return Err("No installation found at that path. Run update first.".into());
    }
    let active = Active::read(&root)?;
    match command {
        "status" => status(&root, &active)?,
        "check" | "stage" | "update" => {
            let available = package::latest()?;
            let is_newer = newer(&available.version, active.current.as_deref());
            println!("Mirror: {} ({})", available.version, available.architecture);
            if !is_newer {
                println!("No newer package than the active version.");
            } else if command != "check" {
                package::stage(&root, &available)?;
                if command == "update" {
                    apply(&root, &active)?;
                }
            }
        }
        "apply" => apply(&root, &active)?,
        "launch" | "health" => {
            let current = active
                .current
                .as_deref()
                .ok_or("No active portable installation")?;
            let exe = installed_exe(&root, current)?;
            if command == "launch" {
                health::launch_command(&exe)?.spawn()?;
            } else {
                health::check(&exe, &root)?;
                println!("Startup check passed for {current}.");
            }
        }
        "rollback" => rollback(&root, &active)?,
        _ => unreachable!(),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_only_after_a_successful_health_check_and_preserves_previous() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old = Active {
            current: Some("26.1117.3838.0".into()),
            previous: None,
        };
        atomic_json(root, "active.json", &old).unwrap();
        let new = "26.1117.4957.0";
        assert!(activate(root, &old, new, || Err("Synthetic startup failure".into())).is_err());
        assert_eq!(Active::read(root).unwrap().current, old.current);
        activate(root, &old, new, || Ok(())).unwrap();
        let state = Active::read(root).unwrap();
        assert_eq!(state.current.as_deref(), Some(new));
        assert_eq!(state.previous, old.current);
    }

    #[test]
    fn refuses_to_take_over_an_existing_nonempty_folder() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("owned-by-user.txt"), "keep").unwrap();
        assert!(open_root(dir.path(), true).is_err());
        assert!(!dir.path().join(MARKER).exists());
    }

    #[test]
    fn rollback_rejects_missing_target_then_switches_to_the_previous_install() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let previous = "26.1117.3838.0";
        let active = Active {
            current: Some("26.1117.4957.0".into()),
            previous: Some(previous.into()),
        };
        atomic_json(root, "active.json", &active).unwrap();
        assert!(rollback(root, &active).is_err());
        assert_eq!(Active::read(root).unwrap().current, active.current);

        let target = versions(root).unwrap().join(previous);
        fs::create_dir_all(target.join("app")).unwrap();
        fs::write(target.join("app/ChatGPT.exe"), "never executed").unwrap();
        fs::write(target.join("AppxManifest.xml"), format!(r#"<Package><Identity Name="OpenAI.Codex" Version="{previous}" ProcessorArchitecture="x64" Publisher="CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B"/><Applications><Application Id="App" Executable="app/ChatGPT.exe"/></Applications></Package>"#)).unwrap();
        atomic_json(&target, GENERATION_FILE, &serde_json::json!({
            "version": previous, "architecture": "x64", "sha256": "a".repeat(64), "size": 3, "tag": "codex-app-26.1117.1"
        })).unwrap();
        rollback(root, &active).unwrap();
        let result = Active::read(root).unwrap();
        assert_eq!(result.current.as_deref(), Some(previous));
        assert_eq!(result.previous, active.current);
    }
}
