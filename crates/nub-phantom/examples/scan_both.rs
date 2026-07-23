//! Compare the filesystem and CAS-index phantom scanners on an extracted package.

use std::io;
use std::path::{Path, PathBuf};

fn index_of(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    fn visit(base: &Path, current: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(base, &path, out)?;
            } else {
                let relative = path
                    .strip_prefix(base)
                    .map_err(io::Error::other)?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative, path));
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).expect("usage: scan_both <dir>");
    let root = Path::new(&root);
    let extracted = nub_phantom_scan::scan_extracted(root);
    let indexed = nub_phantom_scan::scan_index(&index_of(root)?);
    println!("scan_extracted: {extracted:?}");
    println!("scan_index:     {indexed:?}");

    if extracted != indexed {
        return Err("scanner outputs differ".into());
    }
    Ok(())
}
