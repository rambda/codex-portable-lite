use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{atomic_json, read_json, system_program, Result};

const MIRROR: &str = "Wangnov/codex-app-mirror";
const PUBLISHER: &str = "CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B";
const MAX_PACKAGE_BYTES: u64 = 2_000_000_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Package {
    pub version: String,
    pub architecture: String,
    pub sha256: String,
    pub size: u64,
    tag: String,
}

#[derive(Deserialize)]
struct MirrorRelease {
    tag_name: String,
    assets: Vec<MirrorAsset>,
}

#[derive(Deserialize)]
struct MirrorAsset {
    name: String,
    size: u64,
    digest: Option<String>,
}

pub fn version_parts(value: &str) -> Option<[u16; 4]> {
    let parts = value
        .split('.')
        .map(str::parse::<u16>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let parts: [u16; 4] = parts.try_into().ok()?;
    let [a, b, c, d] = parts;
    (format!("{a}.{b}.{c}.{d}") == value).then_some(parts)
}

fn architecture() -> Result<&'static str> {
    use windows_sys::Win32::System::SystemInformation::{
        GetNativeSystemInfo, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64,
        SYSTEM_INFO,
    };
    let mut info = SYSTEM_INFO::default();
    let arch = unsafe {
        GetNativeSystemInfo(&mut info);
        info.Anonymous.Anonymous.wProcessorArchitecture
    };
    match arch {
        PROCESSOR_ARCHITECTURE_AMD64 => Ok("x64"),
        PROCESSOR_ARCHITECTURE_ARM64 => Ok("arm64"),
        _ => Err(format!("Unsupported Windows architecture: {arch}").into()),
    }
}

impl Package {
    pub fn validate(&self) -> Result<()> {
        let tag_version = self.tag.strip_prefix("codex-app-").unwrap_or("");
        let valid_tag = !tag_version.is_empty()
            && tag_version.len() <= 36
            && tag_version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        if version_parts(&self.version).is_none()
            || !matches!(self.architecture.as_str(), "x64" | "arm64")
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.size == 0
            || self.size > MAX_PACKAGE_BYTES
            || !valid_tag
        {
            return Err("Invalid mirror package metadata".into());
        }
        Ok(())
    }

    fn matches_native(&self) -> Result<()> {
        if self.architecture != architecture()? {
            return Err("The staged package is for a different Windows architecture".into());
        }
        Ok(())
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join(format!("{}.msix", self.sha256))
    }

    fn filename(&self) -> String {
        format!(
            "OpenAI.Codex_{}_{}__2p2nqsd0c76g0.Msix",
            self.version, self.architecture
        )
    }
}

fn select_asset(release: MirrorRelease, arch: &str) -> Result<Package> {
    let suffix = format!("_{arch}__2p2nqsd0c76g0.Msix");
    let mut matches = release.assets.into_iter().filter_map(|asset| {
        let version = asset
            .name
            .strip_prefix("OpenAI.Codex_")?
            .strip_suffix(&suffix)?
            .to_string();
        Some((asset, version))
    });
    let (asset, version) = matches
        .next()
        .ok_or("Mirror release has no native Windows MSIX")?;
    if matches.next().is_some() {
        return Err("Mirror release has multiple matching MSIX assets".into());
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("Mirror release does not provide an MSIX SHA-256")?;
    let result = Package {
        version,
        architecture: arch.into(),
        sha256: digest.to_ascii_lowercase(),
        size: asset.size,
        tag: release.tag_name,
    };
    result.validate()?;
    Ok(result)
}

fn curl() -> Result<Command> {
    let mut command = Command::new(system_program("curl.exe")?);
    command.args([
        "--disable",
        "--fail",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
        "--retry",
        "2",
        "--user-agent",
        "codex-portable-lite/0.1",
    ]);
    Ok(command)
}

pub fn latest() -> Result<Package> {
    let endpoint = format!("https://api.github.com/repos/{MIRROR}/releases/latest");
    let output = curl()?
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "25",
            "--max-filesize",
            "1000000",
            &endpoint,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot read mirror release: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    select_asset(serde_json::from_slice(&output.stdout)?, architecture()?)
}

pub fn staged(root: &Path) -> Result<(Package, PathBuf)> {
    let package = read_json::<Package>(&root.join("staged.json"))?
        .ok_or("No staged update. Run stage first.")?;
    package.validate()?;
    package.matches_native()?;
    let path = package.path(root);
    Ok((package, path))
}

pub fn stage(root: &Path, package: &Package) -> Result<()> {
    package.validate()?;
    package.matches_native()?;
    let path = package.path(root);
    let already_verified = if crate::exists(&path)? {
        crate::plain(&path, false)?;
        match verify(&path, package) {
            Ok(_) => true,
            Err(error) => {
                eprintln!(
                    "Cached package did not pass verification; trying the mirror again: {error}"
                );
                false
            }
        }
    } else {
        false
    };
    if !already_verified {
        let temp = tempfile::Builder::new()
            .prefix("download-")
            .tempdir_in(root)?;
        let incoming = temp.path().join("incoming.msix");
        let url = format!(
            "https://github.com/{MIRROR}/releases/download/{}/{}",
            package.tag,
            package.filename()
        );
        eprintln!("Downloading {} from the GitHub mirror…", package.filename());
        let status = curl()?
            .args([
                "--progress-bar",
                "--max-time",
                "900",
                "--max-filesize",
                &package.size.to_string(),
                "--output",
            ])
            .arg(&incoming)
            .arg(&url)
            .status()?;
        if !status.success() {
            return Err(format!("Mirror download failed ({status})").into());
        }
        verify(&incoming, package)?;
        fs::rename(&incoming, &path)?;
    }
    let old = read_json::<Package>(&root.join("staged.json"))?;
    atomic_json(root, "staged.json", package)?;
    if let Some(old) = old.filter(|old| old != package && old.validate().is_ok()) {
        if old.sha256 != package.sha256 {
            let _ = fs::remove_file(old.path(root));
        }
    }
    println!(
        "Staged and verified {} ({})",
        package.version, package.architecture
    );
    Ok(())
}

/// Hold a read-only, non-delete sharing handle until extraction completes.
pub fn verify(path: &Path, package: &Package) -> Result<File> {
    crate::plain(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ
    }
    let mut file = options.open(path)?;
    if file.metadata()?.len() != package.size {
        return Err("MSIX file size does not match the mirror".into());
    }
    let mut sha = Sha256::new();
    io::copy(&mut file, &mut sha)?;
    if !format!("{:x}", sha.finalize()).eq_ignore_ascii_case(&package.sha256) {
        return Err("MSIX SHA-256 does not match the mirror".into());
    }
    verify_signer(path)?;
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    let manifest = archive.by_name("AppxManifest.xml")?;
    if manifest.size() > 1024 * 1024 {
        return Err("Unexpectedly large MSIX manifest".into());
    }
    let mut xml = String::new();
    manifest.take(1024 * 1024).read_to_string(&mut xml)?;
    validate_manifest(&xml, package)?;
    Ok(file)
}

// Adapted from the audited upstream's Authenticode publisher check.
fn verify_signer(path: &Path) -> Result<()> {
    let output = Command::new(system_program(r"WindowsPowerShell\v1.0\powershell.exe")?)
        .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1')
$sig = Get-AuthenticodeSignature -LiteralPath $env:CODEX_PORTABLE_LITE_VERIFY_PATH
@{ status = [string]$sig.Status; subject = [string]$sig.SignerCertificate.Subject; issuer = [string]$sig.SignerCertificate.Issuer } | ConvertTo-Json -Compress
"#]).env("CODEX_PORTABLE_LITE_VERIFY_PATH", path).output()?;
    if !output.status.success() {
        return Err(format!(
            "Authenticode check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    #[derive(Deserialize)]
    struct Signature {
        status: String,
        subject: String,
        issuer: String,
    }
    let sig: Signature = serde_json::from_slice(&output.stdout)?;
    let issuer = sig.issuer.to_ascii_lowercase();
    if sig.status != "Valid"
        || !sig.subject.trim().eq_ignore_ascii_case(PUBLISHER)
        || !issuer
            .split(',')
            .any(|part| part.trim().starts_with("cn=microsoft marketplace ca"))
        || !issuer
            .split(',')
            .any(|part| part.trim() == "o=microsoft corporation")
    {
        return Err(format!("Rejected MSIX signature: {} ({})", sig.status, sig.subject).into());
    }
    Ok(())
}

pub fn validate_manifest(xml: &str, package: &Package) -> Result<String> {
    let doc = roxmltree::Document::parse(xml)?;
    let identity = doc
        .root_element()
        .children()
        .find(|node| node.has_tag_name("Identity"))
        .ok_or("MSIX has no package identity")?;
    if identity.attribute("Name") != Some("OpenAI.Codex")
        || !identity
            .attribute("Publisher")
            .is_some_and(|value| value.eq_ignore_ascii_case(PUBLISHER))
        || identity.attribute("Version") != Some(&package.version)
        || identity.attribute("ProcessorArchitecture") != Some(&package.architecture)
        || identity
            .attribute("ResourceId")
            .is_some_and(|value| !value.is_empty())
    {
        return Err("MSIX identity does not match the expected OpenAI desktop package".into());
    }
    let entry = doc
        .descendants()
        .find(|node| node.has_tag_name("Application") && node.attribute("Id") == Some("App"))
        .and_then(|node| node.attribute("Executable"))
        .ok_or("MSIX has no main application entry")?
        .replace('\\', "/");
    if !matches!(entry.as_str(), "app/ChatGPT.exe" | "app/Codex.exe") {
        return Err(format!("Unexpected codex entry path: {entry}").into());
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Package {
        Package {
            version: "26.1117.4957.0".into(),
            architecture: "x64".into(),
            sha256: "a".repeat(64),
            size: 3,
            tag: "codex-app-26.1117.41037".into(),
        }
    }

    #[test]
    fn selects_only_a_matching_release_asset_with_a_digest() {
        let pkg = example();
        let release = MirrorRelease {
            tag_name: pkg.tag.clone(),
            assets: vec![MirrorAsset {
                name: pkg.filename(),
                size: pkg.size,
                digest: Some(format!("sha256:{}", pkg.sha256)),
            }],
        };
        assert_eq!(select_asset(release, "x64").unwrap(), pkg);
    }

    #[test]
    fn catches_modified_bytes_before_opening_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.msix");
        fs::write(&path, b"bad").unwrap();
        let error = verify(&path, &example()).unwrap_err().to_string();
        assert!(error.contains("SHA-256"), "{error}");
    }

    #[test]
    fn rejects_noncanonical_versions_and_other_publishers() {
        assert!(version_parts("26.01117.4957.0").is_none());
        let xml = format!(
            r#"<Package><Identity Name="OpenAI.Codex" Version="26.1117.4957.0" ProcessorArchitecture="x64" Publisher="{PUBLISHER}"/><Applications><Application Id="App" Executable="app\ChatGPT.exe"/></Applications></Package>"#
        );
        assert_eq!(
            validate_manifest(&xml, &example()).unwrap(),
            "app/ChatGPT.exe"
        );
        assert!(validate_manifest(&xml.replace(PUBLISHER, "CN=Untrusted"), &example()).is_err());
    }
}
