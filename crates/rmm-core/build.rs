//! The hashes of the harness plugins' source, hashed as harness/common/source_hash.cmake does, so a
//! test run can say when the plugin in the game was built from other source than this rmm.
use sha2::{Digest, Sha256};
use std::path::Path;

fn source(root: &Path, game: &str) -> String {
    let mut files = vec![];
    for dir in ["common".to_string(), format!("{game}/src")] {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { return String::new() };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".cpp") || name.ends_with(".h") {
                files.push(format!("{dir}/{name}"));
            }
        }
    }
    files.sort();
    let mut h = Sha256::new();
    for f in &files {
        let Ok(text) = std::fs::read(root.join(f)) else { return String::new() };
        h.update(f.as_bytes());
        h.update(b"\n");
        h.update(text.into_iter().filter(|&b| b != b'\r').collect::<Vec<u8>>());
    }
    format!("{:x}", h.finalize())
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness");
    println!("cargo:rerun-if-changed={}", root.display());
    for game in ["skse", "f4se"] {
        println!("cargo:rustc-env=RMM_HARNESS_{}={}", game.to_uppercase(), source(&root, game));
    }
}
