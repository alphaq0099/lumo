//! Listing and extracting archive members (zip-based containers and rar).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

fn member_is_image(name: &Path) -> bool {
    let Some(base) = name.file_name().map(|f| f.to_string_lossy().to_string()) else {
        return false;
    };
    if base.starts_with('.') || base.starts_with("._") {
        return false; // hidden / resource-fork junk
    }
    match name.extension() {
        Some(e) => crate::doc::is_image_ext(&e.to_string_lossy()),
        None => false,
    }
}

fn natural_sort(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| natord::compare(&a.to_lowercase(), &b.to_lowercase()));
    names
}

/// Ordered image member names of a zip-based container (CBZ / ZIP / EPUB).
pub fn zip_image_names(path: &Path) -> anyhow::Result<Vec<String>> {
    let f = std::fs::File::open(path)?;
    let z = zip::ZipArchive::new(f).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut names: Vec<String> = Vec::new();
    for name in z.file_names() {
        let name = name.map_err(|e| anyhow::anyhow!("{e}"))?;
        let p = Path::new(name.as_ref());
        if !name.contains("__MACOSX") && member_is_image(p) {
            names.push(name.to_string());
        }
    }
    Ok(natural_sort(names))
}

/// Ordered image member names of a rar container (CBR / RAR).
pub fn rar_image_names(path: &Path) -> anyhow::Result<Vec<String>> {
    let listing = unrar::Archive::new(path).open_for_listing().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut names: Vec<String> = Vec::new();
    for entry in listing {
        let header = entry.map_err(|e| anyhow::anyhow!("{e}"))?;
        if header.is_file() {
            let name = header.filename.to_string_lossy().to_string();
            if member_is_image(Path::new(&name)) {
                names.push(name);
            }
        }
    }
    Ok(natural_sort(names))
}

/// All extensions for file dialogs.
pub const ALL_EXTS: &[&str] = crate::doc::IMAGE_EXTS;

/// Zip archive cache for the decode worker, keyed by container path.
pub struct ZipPool {
    pools: HashMap<std::path::PathBuf, zip::ZipArchive<std::fs::File>>,
}

impl Default for ZipPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipPool {
    pub fn new() -> Self {
        Self { pools: HashMap::new() }
    }

    pub fn read_member(&mut self, path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
        let z = match self.pools.get_mut(path) {
            Some(z) => z,
            None => {
                let f = std::fs::File::open(path)?;
                let z = zip::ZipArchive::new(f).map_err(|e| anyhow::anyhow!("{e}"))?;
                self.pools.insert(path.to_path_buf(), z);
                self.pools.get_mut(path).unwrap()
            }
        };
        let mut member = z.by_name(name).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buf = Vec::with_capacity(member.size() as usize);
        member.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Read one member's bytes straight out of a rar container.
pub fn read_rar_member(path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut arc = unrar::Archive::new(path)
        .open_for_processing()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    while let Some(header) = arc.read_header().map_err(|e| anyhow::anyhow!("{e}"))? {
        if header.entry().filename == Path::new(name) {
            let (data, _rest) = header.read().map_err(|e| anyhow::anyhow!("{e}"))?;
            return Ok(data);
        }
        arc = header.skip().map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    anyhow::bail!("Member '{name}' not found in archive")
}
