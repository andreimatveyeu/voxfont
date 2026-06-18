//! Virtual filesystem layer.
//!
//! A [`Location`] names something browsable: today either a real path on disk
//! or a member inside a container archive (only zip is implemented). The rest of
//! the application speaks `Location` instead of `PathBuf`, so directories and
//! archives are navigated and played through the same code paths.
//!
//! ## Adding a new container format later
//!
//! The design is deliberately open. To support, say, `.tar.gz`:
//!   1. Give [`Location`] a new variant (or generalise [`Location::Zip`] to
//!      carry a format tag) that records the archive path plus the inner path.
//!   2. Teach [`is_archive`] to recognise the new suffix so the filesystem
//!      listing presents it as an enterable directory.
//!   3. Implement the same three operations this module already provides for
//!      zip — [`read_dir`] (list one directory level), [`read_bytes`] (read a
//!      member), and [`resolve_to_file`] (materialise a member as a real file
//!      for the libfluidsynth FFI, which can only load by filename).
//!
//! No other module needs to learn about the new format.
//!
//! Note that tar/gzip streams are sequential, not random-access like zip's
//! central directory, so a faithful implementation will likely extract the
//! whole archive once on entry rather than browse it in place — see the
//! performance notes in the project history.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tempfile::NamedTempFile;
use zip::ZipArchive;

/// Where a browsable item lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A path on the real filesystem.
    Fs(PathBuf),
    /// A member inside a zip archive. `archive` is the real path of the `.zip`;
    /// `inner` is the slash-separated path within it, with no trailing slash
    /// (an empty string means the archive root).
    Zip { archive: PathBuf, inner: String },
}

impl Location {
    /// The parent container of this location, if any. Leaving a zip's root
    /// returns to the filesystem directory that holds the archive.
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Fs(p) => p.parent().map(|p| Location::Fs(p.to_path_buf())),
            Location::Zip { archive, inner } => {
                if inner.is_empty() {
                    // Exit the archive, back to the directory containing it.
                    Some(Location::Fs(
                        archive
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from("/")),
                    ))
                } else {
                    let up = match inner.rsplit_once('/') {
                        Some((head, _)) => head.to_string(),
                        None => String::new(),
                    };
                    Some(Location::Zip {
                        archive: archive.clone(),
                        inner: up,
                    })
                }
            }
        }
    }

    /// The display basename: the file/dir name for the player and list rows.
    pub fn file_name(&self) -> String {
        match self {
            Location::Fs(p) => base_name(p),
            Location::Zip { archive, inner } => {
                if inner.is_empty() {
                    base_name(archive)
                } else {
                    inner.rsplit('/').next().unwrap_or(inner).to_string()
                }
            }
        }
    }

    /// A human-readable path for panel titles and prompts.
    pub fn display(&self) -> String {
        match self {
            Location::Fs(p) => p.to_string_lossy().to_string(),
            // "/music/sf.zip:/banks" reads clearly as "inside this archive".
            Location::Zip { archive, inner } => format!("{}:/{}", archive.display(), inner),
        }
    }

    /// The real filesystem path this location maps to, if it is on disk. Used
    /// where only the filesystem makes sense (session persistence, the "go to
    /// directory" prompt). For a zip member this is `None`; for a zip root or
    /// subdir it falls back to the directory that holds the archive.
    pub fn as_fs(&self) -> Option<&Path> {
        match self {
            Location::Fs(p) => Some(p),
            Location::Zip { .. } => None,
        }
    }

    /// The nearest enclosing filesystem directory, always resolvable: the path
    /// itself for an `Fs` dir, or the directory holding the archive otherwise.
    pub fn fs_dir(&self) -> PathBuf {
        match self {
            Location::Fs(p) => p.clone(),
            Location::Zip { archive, .. } => archive
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("/")),
        }
    }

    /// True if this location can be opened as a directory: the filesystem
    /// directory exists, or (for an archive) the `.zip` file exists.
    pub fn is_openable_dir(&self) -> bool {
        match self {
            Location::Fs(p) => p.is_dir(),
            Location::Zip { archive, .. } => archive.is_file(),
        }
    }

    /// True if the backing file is present. An archive member is assumed present
    /// when its archive exists; the member itself is verified only when read.
    pub fn exists(&self) -> bool {
        match self {
            Location::Fs(p) => p.exists(),
            Location::Zip { archive, .. } => archive.is_file(),
        }
    }

    /// Serialise to a single line for the session file. Filesystem paths are
    /// written verbatim, so session files written before archive support remain
    /// readable. A zip location uses a tab-delimited `zip<TAB>archive<TAB>inner`
    /// form, which a plain filesystem path can never collide with.
    pub fn encode(&self) -> String {
        match self {
            Location::Fs(p) => p.to_string_lossy().to_string(),
            Location::Zip { archive, inner } => {
                format!("zip\t{}\t{}", archive.to_string_lossy(), inner)
            }
        }
    }

    /// Inverse of [`Location::encode`]. Anything without the `zip<TAB>` marker is
    /// read as a filesystem path.
    pub fn decode(s: &str) -> Location {
        match s.strip_prefix("zip\t") {
            Some(rest) => {
                let (archive, inner) = rest.split_once('\t').unwrap_or((rest, ""));
                Location::Zip {
                    archive: PathBuf::from(archive),
                    inner: inner.to_string(),
                }
            }
            None => Location::Fs(PathBuf::from(s)),
        }
    }
}

/// One immediate child of a directory location, before file-type filtering.
pub struct DirChild {
    pub name: String,
    pub loc: Location,
    pub is_dir: bool,
    /// Uncompressed size in bytes; 0 for directories and for archive members
    /// (whose size is not read during indexing — see [`ZipIndex`]).
    pub size: u64,
}

/// True if a filesystem file should be presented as an enterable directory.
/// Extension point: add `".tar.gz"`, `".tgz"`, ... here as formats land.
fn is_archive(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".zip")
}

/// Caches per-archive directory indexes so navigating within a zip (or
/// re-entering it) does not re-read its central directory each time. Keyed by
/// archive path; not invalidated on change, so a zip rewritten mid-session may
/// list staleley until the program restarts.
#[derive(Default)]
pub struct ZipCache(HashMap<PathBuf, Rc<ZipIndex>>);

impl ZipCache {
    pub fn new() -> ZipCache {
        ZipCache(HashMap::new())
    }

    fn get_or_build(&mut self, archive: &Path) -> Option<Rc<ZipIndex>> {
        if let Some(idx) = self.0.get(archive) {
            return Some(idx.clone());
        }
        let idx = Rc::new(ZipIndex::build(archive).ok()?);
        self.0.insert(archive.to_path_buf(), idx.clone());
        Some(idx)
    }
}

/// A directory tree distilled from a zip's flat member list, so each directory
/// level can be listed in O(children). Built once per archive from the central
/// directory via `file_names()` only — member sizes are intentionally not read,
/// as that would require a local-header seek per entry (prohibitive for archives
/// with hundreds of thousands of files).
struct ZipIndex {
    /// Maps a directory prefix (`""` for root, otherwise ending in `/`) to its
    /// immediate children, each as `name -> is_dir`.
    children: HashMap<String, HashMap<String, bool>>,
}

impl ZipIndex {
    fn build(archive: &Path) -> Result<ZipIndex, String> {
        let file = File::open(archive).map_err(|e| e.to_string())?;
        let zip = ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut children: HashMap<String, HashMap<String, bool>> = HashMap::new();
        // Ensure the root always lists, even for an empty archive.
        children.entry(String::new()).or_default();

        for full in zip.file_names() {
            let is_dir_marker = full.ends_with('/');
            let trimmed = full.trim_end_matches('/');
            if trimmed.is_empty() {
                continue;
            }
            let parts: Vec<&str> = trimmed.split('/').collect();
            let mut prefix = String::new();
            for (i, part) in parts.iter().enumerate() {
                let is_last = i + 1 == parts.len();
                let child_is_dir = !is_last || is_dir_marker;
                children
                    .entry(prefix.clone())
                    .or_default()
                    .entry((*part).to_string())
                    .and_modify(|d| *d |= child_is_dir)
                    .or_insert(child_is_dir);
                prefix.push_str(part);
                prefix.push('/');
                if child_is_dir {
                    children.entry(prefix.clone()).or_default();
                }
            }
        }
        Ok(ZipIndex { children })
    }
}

/// List the immediate children of a directory location. The returned children
/// are unsorted and unfiltered; the browser applies hidden-file and extension
/// filtering. When `allow_archives` is true, a filesystem `.zip` file appears
/// here as a directory whose location is the archive root; when false it is
/// reported as an ordinary file (and so filtered out unless its extension
/// matches), which is how the SoundFont panel keeps archives out.
pub fn read_dir(dir: &Location, cache: &mut ZipCache, allow_archives: bool) -> Vec<DirChild> {
    match dir {
        Location::Fs(p) => read_dir_fs(p, allow_archives),
        Location::Zip { archive, inner } => read_dir_zip(archive, inner, cache),
    }
}

fn read_dir_fs(dir: &Path, allow_archives: bool) -> Vec<DirChild> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let path = ent.path();
            if path.is_dir() {
                out.push(DirChild {
                    name,
                    loc: Location::Fs(path),
                    is_dir: true,
                    size: 0,
                });
            } else if allow_archives && is_archive(&name) {
                // Present the archive as a directory the user can step into.
                out.push(DirChild {
                    name,
                    loc: Location::Zip {
                        archive: path,
                        inner: String::new(),
                    },
                    is_dir: true,
                    size: 0,
                });
            } else {
                let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(DirChild {
                    name,
                    loc: Location::Fs(path),
                    is_dir: false,
                    size,
                });
            }
        }
    }
    out
}

fn read_dir_zip(archive: &Path, inner: &str, cache: &mut ZipCache) -> Vec<DirChild> {
    let idx = match cache.get_or_build(archive) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let key = if inner.is_empty() {
        String::new()
    } else {
        format!("{inner}/")
    };
    let mut out = Vec::new();
    if let Some(map) = idx.children.get(&key) {
        for (name, is_dir) in map {
            let child_inner = if inner.is_empty() {
                name.clone()
            } else {
                format!("{inner}/{name}")
            };
            // A zip nested inside a zip is left as an ordinary (filtered) file
            // for now; nested-archive entry is not supported.
            out.push(DirChild {
                name: name.clone(),
                loc: Location::Zip {
                    archive: archive.to_path_buf(),
                    inner: child_inner,
                },
                is_dir: *is_dir,
                size: 0,
            });
        }
    }
    out
}

/// Read the full bytes of a file location.
pub fn read_bytes(loc: &Location) -> Result<Vec<u8>, String> {
    match loc {
        Location::Fs(p) => std::fs::read(p).map_err(|e| e.to_string()),
        Location::Zip { archive, inner } => {
            let file = File::open(archive).map_err(|e| e.to_string())?;
            let mut zip = ZipArchive::new(file).map_err(|e| e.to_string())?;
            let mut entry = zip
                .by_name(inner)
                .map_err(|_| format!("not found in archive: {inner}"))?;
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            Ok(buf)
        }
    }
}

/// Resolve a file location to a real path the libfluidsynth FFI can open.
///
/// For a filesystem file this is the path itself, with no temp file. For an
/// archive member the bytes are extracted to a temp file; the returned guard
/// must be kept alive for as long as the path may be read (fluidsynth loads
/// MIDI lazily on the audio thread, so the file must outlive the `play` call).
/// The member's extension is preserved so any suffix-based format detection
/// still works.
pub fn resolve_to_file(loc: &Location) -> Result<(PathBuf, Option<NamedTempFile>), String> {
    match loc {
        Location::Fs(p) => Ok((p.clone(), None)),
        Location::Zip { inner, .. } => {
            let bytes = read_bytes(loc)?;
            let suffix = match Path::new(inner).extension().and_then(|e| e.to_str()) {
                Some(e) => format!(".{e}"),
                None => String::new(),
            };
            let mut tf = tempfile::Builder::new()
                .prefix("voxfont-")
                .suffix(&suffix)
                .tempfile()
                .map_err(|e| e.to_string())?;
            tf.write_all(&bytes).map_err(|e| e.to_string())?;
            tf.flush().map_err(|e| e.to_string())?;
            let path = tf.path().to_path_buf();
            Ok((path, Some(tf)))
        }
    }
}

/// Basename of a path as a `String`, falling back to the whole path.
fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    /// Write a zip with the given member names (dirs end in '/') to `path`.
    fn make_zip(path: &Path, names: &[&str]) {
        let f = File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for n in names {
            if n.ends_with('/') {
                w.add_directory(n.trim_end_matches('/'), opts).unwrap();
            } else {
                w.start_file(*n, opts).unwrap();
                w.write_all(b"data").unwrap();
            }
        }
        w.finish().unwrap();
    }

    fn child_names(dir: &Location, cache: &mut ZipCache) -> Vec<(String, bool)> {
        let mut v: Vec<(String, bool)> = read_dir(dir, cache, true)
            .into_iter()
            .map(|c| (c.name, c.is_dir))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn lists_zip_root_and_subdirs() {
        let d = tempdir().unwrap();
        let zip = d.path().join("songs.zip");
        // Note: "a/" has no explicit dir entry; it must be inferred from a/x.mid.
        make_zip(&zip, &["a/x.mid", "a/b/y.mid", "top.mid", "empty/"]);
        let mut cache = ZipCache::new();

        let root = Location::Zip {
            archive: zip.clone(),
            inner: String::new(),
        };
        assert_eq!(
            child_names(&root, &mut cache),
            [
                ("a".to_string(), true),
                ("empty".to_string(), true),
                ("top.mid".to_string(), false),
            ]
        );

        let a = Location::Zip {
            archive: zip.clone(),
            inner: "a".to_string(),
        };
        assert_eq!(
            child_names(&a, &mut cache),
            [("b".to_string(), true), ("x.mid".to_string(), false)]
        );
    }

    #[test]
    fn parent_navigates_out_of_archive() {
        let d = tempdir().unwrap();
        let zip = d.path().join("s.zip");

        let sub = Location::Zip {
            archive: zip.clone(),
            inner: "a/b".to_string(),
        };
        assert_eq!(
            sub.parent(),
            Some(Location::Zip {
                archive: zip.clone(),
                inner: "a".to_string()
            })
        );
        let top = Location::Zip {
            archive: zip.clone(),
            inner: "a".to_string(),
        };
        assert_eq!(
            top.parent(),
            Some(Location::Zip {
                archive: zip.clone(),
                inner: String::new()
            })
        );
        // Leaving the root drops back to the filesystem dir holding the archive.
        let root = Location::Zip {
            archive: zip.clone(),
            inner: String::new(),
        };
        assert_eq!(root.parent(), Some(Location::Fs(d.path().to_path_buf())));
    }

    #[test]
    fn fs_listing_shows_zip_as_directory() {
        let d = tempdir().unwrap();
        let zip = d.path().join("bank.zip");
        make_zip(&zip, &["inside.mid"]);
        let mut cache = ZipCache::new();
        let dir = Location::Fs(d.path().to_path_buf());
        let z = read_dir(&dir, &mut cache, true)
            .into_iter()
            .find(|c| c.name == "bank.zip")
            .unwrap();
        assert!(z.is_dir, "a .zip should be presented as a directory");
        assert_eq!(
            z.loc,
            Location::Zip {
                archive: zip,
                inner: String::new()
            }
        );

        // With archives disabled (the SoundFont panel), the .zip is reported as
        // an ordinary file instead, so extension filtering can drop it.
        let z = read_dir(&dir, &mut cache, false)
            .into_iter()
            .find(|c| c.name == "bank.zip")
            .unwrap();
        assert!(!z.is_dir);
        assert!(matches!(z.loc, Location::Fs(_)));
    }

    #[test]
    fn reads_and_resolves_member_bytes() {
        let d = tempdir().unwrap();
        let zip = d.path().join("s.zip");
        make_zip(&zip, &["dir/song.mid"]);
        let loc = Location::Zip {
            archive: zip,
            inner: "dir/song.mid".to_string(),
        };
        assert_eq!(read_bytes(&loc).unwrap(), b"data");

        let (path, guard) = resolve_to_file(&loc).unwrap();
        assert!(guard.is_some(), "archive member needs a temp file");
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some("mid"),
            "extension should be preserved for format detection"
        );
    }

    #[test]
    fn encode_decode_round_trips() {
        let cases = [
            Location::Fs(PathBuf::from("/home/me/midi")),
            Location::Zip {
                archive: PathBuf::from("/m/songs.zip"),
                inner: "dir/a.mid".to_string(),
            },
            // Archive root: empty inner.
            Location::Zip {
                archive: PathBuf::from("/m/songs.zip"),
                inner: String::new(),
            },
        ];
        for loc in cases {
            assert_eq!(Location::decode(&loc.encode()), loc, "{loc:?}");
        }
        // A plain path (as written by older versions) decodes to Fs.
        assert_eq!(
            Location::decode("/old/style/path"),
            Location::Fs(PathBuf::from("/old/style/path"))
        );
    }

    #[test]
    fn file_name_and_display() {
        let loc = Location::Zip {
            archive: PathBuf::from("/m/songs.zip"),
            inner: "a/b/y.mid".to_string(),
        };
        assert_eq!(loc.file_name(), "y.mid");
        assert_eq!(loc.display(), "/m/songs.zip:/a/b/y.mid");

        let root = Location::Zip {
            archive: PathBuf::from("/m/songs.zip"),
            inner: String::new(),
        };
        assert_eq!(root.file_name(), "songs.zip");
    }
}
