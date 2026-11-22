// Adapted from Wangnov/Codex-App-Manager. See LICENSE and NOTICE.
use crate::EngineError;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};
fn io_err(context: &str, err: impl std::fmt::Display) -> EngineError {
    EngineError::Io(format!("{context}: {err}"))
}

const MAX_BLOCK_MAP_BYTES: u64 = 32 * 1024 * 1024;

struct BlockMapEntry {
    logical_name: String,
    output_path: PathBuf,
    size: u64,
}

struct BlockMapPaths {
    by_logical_name: HashMap<String, BlockMapEntry>,
    remaining: HashSet<String>,
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn validate_windows_path_component(component: &str, source: &str) -> Result<(), EngineError> {
    if component.is_empty() || component == "." || component == ".." {
        return Err(EngineError::Msix(format!(
            "unsafe MSIX path component {component:?} in {source}"
        )));
    }
    if component.ends_with([' ', '.']) {
        return Err(EngineError::Msix(format!(
            "MSIX path component has a Windows-unsafe trailing space/dot in {source}: {component:?}"
        )));
    }
    if component.chars().any(|ch| {
        ch == '\0'
            || ch.is_control()
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err(EngineError::Msix(format!(
            "MSIX path component contains a Windows-unsafe character in {source}: {component:?}"
        )));
    }
    if is_windows_reserved_component(component) {
        return Err(EngineError::Msix(format!(
            "MSIX path uses a reserved Windows device name in {source}: {component:?}"
        )));
    }
    Ok(())
}

fn path_from_components(components: &[String]) -> PathBuf {
    components.iter().fold(PathBuf::new(), |mut path, part| {
        path.push(part);
        path
    })
}

fn windows_path_key(components: &[String]) -> String {
    components
        .iter()
        .map(|component| component.to_lowercase())
        .collect::<Vec<_>>()
        .join("\\")
}

fn parse_logical_msix_path(name: &str) -> Result<Vec<String>, EngineError> {
    if name.is_empty() || name.starts_with(['\\', '/']) || name.contains('/') {
        return Err(EngineError::Msix(format!(
            "invalid AppxBlockMap logical path: {name:?}"
        )));
    }
    let components = name.split('\\').map(str::to_string).collect::<Vec<_>>();
    for component in &components {
        validate_windows_path_component(component, name)?;
    }
    Ok(components)
}

fn parse_appx_block_map(xml: &str) -> Result<BlockMapPaths, EngineError> {
    let document = roxmltree::Document::parse(xml)
        .map_err(|err| EngineError::Msix(format!("AppxBlockMap.xml: {err}")))?;
    let mut by_logical_name = HashMap::new();
    let mut windows_names = HashMap::<String, String>::new();

    for file in document
        .descendants()
        .filter(|node| node.has_tag_name("File"))
    {
        let logical_name = file
            .attribute("Name")
            .ok_or_else(|| EngineError::Msix("AppxBlockMap File missing Name".to_string()))?
            .to_string();
        let size = file
            .attribute("Size")
            .ok_or_else(|| {
                EngineError::Msix(format!("AppxBlockMap File missing Size: {logical_name}"))
            })?
            .parse::<u64>()
            .map_err(|err| {
                EngineError::Msix(format!(
                    "AppxBlockMap File has invalid Size for {logical_name}: {err}"
                ))
            })?;
        let components = parse_logical_msix_path(&logical_name)?;
        let windows_key = windows_path_key(&components);
        if let Some(previous) = windows_names.insert(windows_key, logical_name.clone()) {
            return Err(EngineError::Msix(format!(
                "AppxBlockMap paths collide on Windows: {previous:?} and {logical_name:?}"
            )));
        }
        let canonical_name = components.join("\\");
        let entry = BlockMapEntry {
            logical_name: logical_name.clone(),
            output_path: path_from_components(&components),
            size,
        };
        if by_logical_name.insert(canonical_name, entry).is_some() {
            return Err(EngineError::Msix(format!(
                "duplicate AppxBlockMap path: {logical_name}"
            )));
        }
    }

    if by_logical_name.is_empty() {
        return Err(EngineError::Msix(
            "AppxBlockMap.xml contains no payload files".to_string(),
        ));
    }
    let remaining = by_logical_name.keys().cloned().collect();
    Ok(BlockMapPaths {
        by_logical_name,
        remaining,
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_zip_component(component: &str, source: &str) -> Result<(String, bool), EngineError> {
    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte));
        let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte));
        let (Some(high), Some(low)) = (high, low) else {
            return Err(EngineError::Msix(format!(
                "invalid percent escape in MSIX ZIP path: {source:?}"
            )));
        };
        let value = (high << 4) | low;
        if matches!(value, 0 | b'/' | b'\\') {
            return Err(EngineError::Msix(format!(
                "encoded path separator/NUL is forbidden in MSIX ZIP path: {source:?}"
            )));
        }
        decoded.push(value);
        changed = true;
        index += 3;
    }
    let decoded = String::from_utf8(decoded).map_err(|err| {
        EngineError::Msix(format!(
            "percent-decoded MSIX ZIP path is not UTF-8 ({source:?}): {err}"
        ))
    })?;
    validate_windows_path_component(&decoded, source)?;
    Ok((decoded, changed))
}

fn decode_zip_entry_path(name: &str) -> Result<(Vec<String>, bool), EngineError> {
    if name.is_empty() || name.starts_with(['/', '\\']) || name.contains('\\') {
        return Err(EngineError::Msix(format!(
            "invalid MSIX ZIP entry path: {name:?}"
        )));
    }
    let name = name.strip_suffix('/').unwrap_or(name);
    let mut changed = false;
    let mut components = Vec::new();
    for component in name.split('/') {
        let (decoded, component_changed) = decode_zip_component(component, name)?;
        changed |= component_changed;
        components.push(decoded);
    }
    Ok((components, changed))
}

fn read_appx_block_map(
    zip: &mut zip::ZipArchive<fs::File>,
) -> Result<Option<BlockMapPaths>, EngineError> {
    let mut block_map = match zip.by_name("AppxBlockMap.xml") {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(EngineError::Msix(format!("read AppxBlockMap.xml: {err}"))),
    };
    if block_map.size() > MAX_BLOCK_MAP_BYTES {
        return Err(EngineError::Msix(format!(
            "AppxBlockMap.xml is unexpectedly large: {} bytes",
            block_map.size()
        )));
    }
    let mut xml = String::new();
    block_map
        .read_to_string(&mut xml)
        .map_err(|err| EngineError::Msix(format!("decode AppxBlockMap.xml: {err}")))?;
    parse_appx_block_map(&xml).map(Some)
}

pub fn extract_msix(msix_path: &Path, dest: &Path) -> Result<String, EngineError> {
    let file = fs::File::open(msix_path).map_err(|e| io_err("open MSIX", e))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| EngineError::Msix(format!("open zip: {e}")))?;
    let mut block_map = read_appx_block_map(&mut zip)?;
    let mut manifest_xml = None;
    let mut written_paths = HashMap::<String, String>::new();
    let mut logical_path_remaps = 0usize;

    for idx in 0..zip.len() {
        let mut file = zip
            .by_index(idx)
            .map_err(|e| EngineError::Msix(format!("read zip entry {idx}: {e}")))?;
        let zip_name = crate::archive::entry_name(&file)
            .map_err(|e| EngineError::Msix(format!("decode zip entry {idx} name: {e}")))?
            .into_owned();
        let (decoded_components, was_percent_encoded) = decode_zip_entry_path(&zip_name)?;
        if file.is_dir() {
            continue;
        }

        let decoded_name = decoded_components.join("\\");
        let output_path = if let Some(paths) = block_map.as_mut() {
            if let Some(entry) = paths.by_logical_name.get(&decoded_name) {
                if file.size() != entry.size {
                    return Err(EngineError::Msix(format!(
                        "MSIX payload size disagrees with AppxBlockMap for {}: zip={} blockMap={}",
                        entry.logical_name,
                        file.size(),
                        entry.size
                    )));
                }
                paths.remaining.remove(&decoded_name);
                if was_percent_encoded {
                    logical_path_remaps += 1;
                }
                entry.output_path.clone()
            } else {
                if was_percent_encoded {
                    return Err(EngineError::Msix(format!(
                        "percent-encoded MSIX ZIP entry is not described by AppxBlockMap.xml: {zip_name}"
                    )));
                }
                path_from_components(&decoded_components)
            }
        } else {
            if was_percent_encoded {
                return Err(EngineError::Msix(format!(
                    "cannot safely map percent-encoded MSIX ZIP entry without AppxBlockMap.xml: {zip_name}"
                )));
            }
            path_from_components(&decoded_components)
        };
        let output_components = output_path
            .iter()
            .map(|component| component.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let output_key = windows_path_key(&output_components);
        if let Some(previous) = written_paths.insert(output_key, zip_name.clone()) {
            return Err(EngineError::Msix(format!(
                "MSIX entries collide on Windows after logical path mapping: {previous:?} and {zip_name:?}"
            )));
        }

        let out_path = dest.join(&output_path);
        if !out_path.starts_with(dest) {
            return Err(EngineError::Msix(format!(
                "MSIX logical path escaped extraction root: {}",
                output_path.display()
            )));
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err("create extracted parent", e))?;
        }
        let mut out =
            fs::File::create(&out_path).map_err(|e| io_err("create extracted file", e))?;
        std::io::copy(&mut file, &mut out).map_err(|e| io_err("extract file", e))?;

        if output_path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("AppxManifest.xml"))
            && output_path.components().count() == 1
        {
            let mut xml = String::new();
            fs::File::open(&out_path)
                .and_then(|mut f| f.read_to_string(&mut xml))
                .map_err(|e| io_err("read extracted AppxManifest.xml", e))?;
            manifest_xml = Some(xml);
        }
    }

    if let Some(paths) = block_map {
        if !paths.remaining.is_empty() {
            let mut missing = paths.remaining.into_iter().collect::<Vec<_>>();
            missing.sort();
            let preview = missing
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(EngineError::Msix(format!(
                "MSIX is missing {} payload file(s) declared by AppxBlockMap.xml: {preview}",
                missing.len()
            )));
        }
        log::info!(
            "portable MSIX extraction used AppxBlockMap logical paths remapped_entries={logical_path_remaps}"
        );
    }

    manifest_xml.ok_or_else(|| EngineError::Msix("MSIX missing AppxManifest.xml".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn restores_scoped_names_from_the_block_map() {
        let temp = tempfile::tempdir().unwrap();
        let msix = temp.path().join("fixture.msix");
        let dest = temp.path().join("extracted");
        fs::create_dir(&dest).unwrap();
        let mut writer = zip::ZipWriter::new(fs::File::create(&msix).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            ("AppxManifest.xml", "<Package />"),
            (
                "AppxBlockMap.xml",
                r#"<BlockMap><File Name="app\node_modules\@scope\$file" Size="3"/></BlockMap>"#,
            ),
            ("app/node_modules/%40scope/%24file", "abc"),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(extract_msix(&msix, &dest).unwrap(), "<Package />");
        assert_eq!(
            fs::read(dest.join("app/node_modules/@scope/$file")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn rejects_a_percent_encoded_parent() {
        assert!(decode_zip_entry_path("app/%2e%2e/file").is_err());
    }
}
