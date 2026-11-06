//! SigmaOS-sysroot — root filesystem staging for OS images.
//!
//! Assembles the directory tree that becomes the bootable system:
//! ```text
//! <sysroot>/
//!   boot/vmlinuz-<triple>          kernel image
//!   boot/initramfs-<triple>.img    initramfs archive
//!   usr/bin/…                      userland binaries (coreutils…)
//!   usr/lib/…                      libraries
//!   etc/…                          configuration
//! ```
//!
//! [`Sysroot::stage_file`] copies artifacts into place with collision
//! checks; [`Sysroot::manifest`] emits the staged file list for
//! reproducible image builds and SBOM generation.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Errors — fail-fast on staging problems.
#[derive(Debug)]
pub enum SysrootError {
    Io(io::Error),
    /// Source file doesn't exist.
    MissingSource(PathBuf),
    /// Two artifacts claim the same destination with different content.
    Collision(PathBuf),
    /// Destination escapes the sysroot (absolute/`..` target).
    UnsafeTarget(String),
}

impl std::fmt::Display for SysrootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SysrootError::Io(e) => write!(f, "sysroot i/o error: {e}"),
            SysrootError::MissingSource(p) => write!(f, "missing source: {}", p.display()),
            SysrootError::Collision(p) => write!(f, "destination collision: {}", p.display()),
            SysrootError::UnsafeTarget(t) => write!(f, "unsafe target path: {t}"),
        }
    }
}
impl std::error::Error for SysrootError {}
impl From<io::Error> for SysrootError {
    fn from(e: io::Error) -> Self {
        SysrootError::Io(e)
    }
}

/// Well-known sysroot directories.
pub mod dirs {
    pub const BOOT: &str = "boot";
    pub const USR_BIN: &str = "usr/bin";
    pub const USR_LIB: &str = "usr/lib";
    pub const USR_SHARE: &str = "usr/share";
    pub const ETC: &str = "etc";
    pub const LIB_FIRMWARE: &str = "lib/firmware";
}

/// A staged artifact record — one row of the image manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Path relative to sysroot root, `/`-separated.
    pub dest: String,
    /// Absolute source path it was staged from.
    pub source: PathBuf,
    /// Size in bytes.
    pub size: u64,
}

/// Sysroot staging area.
#[derive(Debug)]
pub struct Sysroot {
    root: PathBuf,
    staged: BTreeMap<String, ManifestEntry>,
}

impl Sysroot {
    /// Open (create if needed) a sysroot at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SysrootError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root, staged: BTreeMap::new() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stage `source` file to `dest_rel` (relative, `/`-separated).
    /// Idempotent for identical content; differing content at the same
    /// dest fails fast.
    pub fn stage_file(&mut self, source: &Path, dest_rel: &str) -> Result<(), SysrootError> {
        validate_dest(dest_rel)?;
        if !source.is_file() {
            return Err(SysrootError::MissingSource(source.to_path_buf()));
        }
        let dest = self.root.join(dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = fs::read(source)?;
        if dest.exists() {
            let existing = fs::read(&dest)?;
            if existing != data {
                return Err(SysrootError::Collision(dest));
            }
        } else {
            fs::write(&dest, &data)?;
        }
        self.staged.insert(
            dest_rel.to_string(),
            ManifestEntry {
                dest: dest_rel.to_string(),
                source: source.to_path_buf(),
                size: data.len() as u64,
            },
        );
        Ok(())
    }

    /// Stage a whole directory recursively under `dest_rel`.
    /// Returns total staged entries in the manifest afterwards.
    pub fn stage_dir(&mut self, source: &Path, dest_rel: &str) -> Result<usize, SysrootError> {
        if !source.is_dir() {
            return Err(SysrootError::MissingSource(source.to_path_buf()));
        }
        stage_walk(source, dest_rel, self)?;
        Ok(self.staged.len())
    }

    /// Emit the sorted manifest (dest order) for reproducible builds.
    pub fn manifest(&self) -> Vec<&ManifestEntry> {
        self.staged.values().collect()
    }

    /// Total staged bytes.
    pub fn total_bytes(&self) -> u64 {
        self.staged.values().map(|e| e.size).sum()
    }

    /// Write a `MANIFEST` file into the sysroot root listing all entries.
    pub fn write_manifest_file(&self) -> Result<PathBuf, SysrootError> {
        let mut s = String::from("# SigmaOS sysroot manifest\n");
        for e in self.manifest() {
            s.push_str(&format!("{}\t{}\n", e.size, e.dest));
        }
        let p = self.root.join("MANIFEST");
        fs::write(&p, s)?;
        Ok(p)
    }
}

fn validate_dest(dest: &str) -> Result<(), SysrootError> {
    if dest.is_empty()
        || dest.starts_with('/')
        || dest.split('/').any(|c| c == "..")
        || dest.contains('\0')
    {
        return Err(SysrootError::UnsafeTarget(dest.to_string()));
    }
    Ok(())
}

fn stage_walk(
    dir: &Path,
    dest_prefix: &str,
    sys: &mut Sysroot,
) -> Result<(), SysrootError> {
    let mut children: Vec<PathBuf> = fs::read_dir(dir)?.map(|e| e.unwrap().path()).collect();
    children.sort();
    for child in children {
        let name = child.file_name().unwrap().to_string_lossy().replace('\\', "/");
        let dest = if dest_prefix.is_empty() {
            name
        } else {
            format!("{dest_prefix}/{name}")
        };
        if child.is_dir() {
            stage_walk(&child, &dest, sys)?;
        } else if child.is_file() {
            sys.stage_file(&child, &dest)?;
        }
        // Non-regular files (sockets, fifos) are skipped — documented
        // limitation; device nodes belong to devtmpfs at runtime.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sysroot-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stage_file_lands_in_layout() {
        let src = tmpdir("src");
        let root = tmpdir("root");
        fs::write(src.join("vmlinuz"), b"kernel").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        s.stage_file(&src.join("vmlinuz"), "boot/vmlinuz-x86_64").unwrap();
        assert!(root.join("boot/vmlinuz-x86_64").exists());
        assert_eq!(s.manifest().len(), 1);
        assert_eq!(s.total_bytes(), 6);
    }

    #[test]
    fn identical_restage_is_idempotent() {
        let src = tmpdir("src2");
        let root = tmpdir("root2");
        fs::write(src.join("f"), b"same").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        s.stage_file(&src.join("f"), "usr/bin/f").unwrap();
        s.stage_file(&src.join("f"), "usr/bin/f").unwrap();
        assert_eq!(s.manifest().len(), 1);
    }

    #[test]
    fn conflicting_content_fails() {
        let src = tmpdir("src3");
        let root = tmpdir("root3");
        fs::write(src.join("a"), b"one").unwrap();
        fs::write(src.join("b"), b"two!").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        s.stage_file(&src.join("a"), "x").unwrap();
        assert!(matches!(
            s.stage_file(&src.join("b"), "x"),
            Err(SysrootError::Collision(_))
        ));
    }

    #[test]
    fn unsafe_dest_rejected() {
        let src = tmpdir("src4");
        let root = tmpdir("root4");
        fs::write(src.join("f"), b"x").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        assert!(matches!(
            s.stage_file(&src.join("f"), "../escape"),
            Err(SysrootError::UnsafeTarget(_))
        ));
        assert!(matches!(
            s.stage_file(&src.join("f"), "/abs"),
            Err(SysrootError::UnsafeTarget(_))
        ));
    }

    #[test]
    fn stage_dir_recursive() {
        let src = tmpdir("src5");
        let root = tmpdir("root5");
        fs::create_dir_all(src.join("usr/bin")).unwrap();
        fs::write(src.join("usr/bin/ls"), b"ls").unwrap();
        fs::write(src.join("usr/bin/cat"), b"cat").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        s.stage_dir(&src.join("usr"), "usr").unwrap();
        assert!(root.join("usr/bin/ls").exists());
        assert!(root.join("usr/bin/cat").exists());
        assert_eq!(s.manifest().len(), 2);
    }

    #[test]
    fn manifest_file_written() {
        let src = tmpdir("src6");
        let root = tmpdir("root6");
        fs::write(src.join("f"), b"data").unwrap();
        let mut s = Sysroot::open(&root).unwrap();
        s.stage_file(&src.join("f"), "etc/f").unwrap();
        let p = s.write_manifest_file().unwrap();
        let content = fs::read_to_string(p).unwrap();
        assert!(content.contains("etc/f"));
    }
}
