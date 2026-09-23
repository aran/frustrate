//! What `snapshot` records about the launch build, and reading it back.
//!
//! The state directory outlives the build outputs it describes: the next
//! reload rebuilds the library and its IR in place, so everything `patch`
//! compares against is copied or summarized here at launch.

use crate::debuginfo::Layouts;
use crate::patch::{Baseline, StaticShape};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: &Path) -> State {
        State {
            dir: dir.to_path_buf(),
        }
    }

    pub fn library(&self) -> PathBuf {
        self.dir.join("library")
    }

    pub fn contract(&self) -> PathBuf {
        self.dir.join("contract")
    }

    fn baseline_path(&self) -> PathBuf {
        self.dir.join("baseline")
    }

    fn inputs_path(&self) -> PathBuf {
        self.dir.join("inputs")
    }

    fn builder_path(&self) -> PathBuf {
        self.dir.join("builder")
    }

    /// Record which build of the patch builder wrote this state.
    pub fn write_builder(&self) -> Result<(), String> {
        write(&self.builder_path(), format!("{:016x}\n", builder_identity()?).as_bytes())
    }

    /// Whether the state was written by this build of the patch builder. What
    /// is recorded is summarized in the builder's own terms, so another build's
    /// record cannot be compared.
    pub fn written_by_this_builder(&self) -> Result<bool, String> {
        let recorded = read_string(&self.builder_path())?;
        Ok(recorded.trim() == format!("{:016x}", builder_identity()?))
    }

    pub fn write_baseline(&self, b: &Baseline) -> Result<(), String> {
        let mut out = String::new();
        for (name, h) in &b.hashes {
            let _ = writeln!(out, "H\t{h:016x}\t{name}");
        }
        for (name, s) in &b.statics {
            let _ = writeln!(out, "S\t{:016x}\t{:016x}\t{name}", s.value_type, s.initializer);
        }
        for (id, (h, display)) in &b.layouts.types {
            let _ = writeln!(out, "T\t{h:016x}\t{id}\t{}", display.replace(['\t', '\n'], " "));
        }
        write(&self.baseline_path(), out.as_bytes())
    }

    pub fn read_baseline(&self) -> Result<Baseline, String> {
        let text = read_string(&self.baseline_path())?;
        let mut hashes = HashMap::new();
        let mut statics = HashMap::new();
        let mut types = HashMap::new();
        for line in text.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            let hex = |s: &str| u64::from_str_radix(s, 16).map_err(|e| format!("corrupt state: {e}"));
            match parts.as_slice() {
                ["H", h, name] => {
                    hashes.insert(name.to_string(), hex(h)?);
                }
                ["S", t, i, name] => {
                    statics.insert(
                        name.to_string(),
                        StaticShape {
                            value_type: hex(t)?,
                            initializer: hex(i)?,
                        },
                    );
                }
                ["T", h, id, display] => {
                    types.insert(id.to_string(), (hex(h)?, display.to_string()));
                }
                _ => return Err(format!("corrupt state line: {line}")),
            }
        }
        Ok(Baseline {
            hashes,
            statics,
            layouts: Layouts { types },
        })
    }

    fn ids_path(&self) -> PathBuf {
        self.dir.join("ids")
    }

    /// Record which member each dispatch id has named, for the life of this
    /// process. Additive: the launch contract's members, then every member of
    /// every contract a patch was built from.
    ///
    /// This is what makes "an id names one member while this app runs" a fact
    /// rather than a probability. Codegen guarantees uniqueness *within* one
    /// interface, and cannot see further: an id freed by removing a member can
    /// be taken by a member added two reloads later, and Dart that still holds
    /// the old meaning — a reload whose Dart never compiled — would reach the
    /// new member with the old member's arguments. Nothing on the wire could
    /// notice. So the ledger spans builds, where the one build codegen sees
    /// cannot.
    pub fn record_ids(&self, ids: &[(u32, String)]) -> Result<(), String> {
        let mut known = self.read_ids()?;
        for (id, member) in ids {
            known.entry(*id).or_insert_with(|| member.clone());
        }
        let mut out = String::new();
        for (id, member) in &known {
            let _ = writeln!(out, "{id:08x}\t{member}");
        }
        write(&self.ids_path(), out.as_bytes())
    }

    /// Every id this process has seen, and the member it named. Empty before
    /// the first `snapshot` writes one.
    pub fn read_ids(&self) -> Result<std::collections::BTreeMap<u32, String>, String> {
        if !self.ids_path().exists() {
            return Ok(Default::default());
        }
        read_string(&self.ids_path())?
            .lines()
            .map(|line| match line.split_once('\t') {
                Some((id, member)) => Ok((
                    u32::from_str_radix(id, 16).map_err(|e| format!("corrupt state: {e}"))?,
                    member.to_string(),
                )),
                None => Err(format!("corrupt state line: {line}")),
            })
            .collect()
    }

    /// The ids in `ids` that named a different member earlier in this process,
    /// as sentences. Empty means no id has changed meaning.
    pub fn reused_ids(&self, ids: &[(u32, String)]) -> Result<Vec<String>, String> {
        let known = self.read_ids()?;
        Ok(ids
            .iter()
            .filter_map(|(id, member)| match known.get(id) {
                Some(was) if was != member => Some(format!(
                    "`{member}` takes the dispatch id `{was}` had in this app, so a caller that still has `{was}` would reach it"
                )),
                _ => None,
            })
            .collect())
    }

    pub fn write_inputs(&self, digests: &[Digest]) -> Result<(), String> {
        let mut out = String::new();
        for d in digests {
            let _ = writeln!(out, "{:016x}\t{}\t{}\t{}", d.content, d.size, d.modified, d.path);
        }
        write(&self.inputs_path(), out.as_bytes())
    }

    pub fn read_inputs(&self) -> Result<Vec<Digest>, String> {
        let text = read_string(&self.inputs_path())?;
        text.lines()
            .map(|line| {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                match parts.as_slice() {
                    [c, s, m, p] => Ok(Digest {
                        content: u64::from_str_radix(c, 16).map_err(|e| e.to_string())?,
                        size: s.parse().map_err(|e| format!("corrupt state: {e}"))?,
                        modified: m.parse().map_err(|e| format!("corrupt state: {e}"))?,
                        path: p.to_string(),
                    }),
                    _ => Err(format!("corrupt state line: {line}")),
                }
            })
            .collect()
    }
}

/// One input file of the library's compilation, identified by content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub path: String,
    pub size: u64,
    pub modified: u128,
    pub content: u64,
}

/// Digest every file under `paths` (directories are walked). `previous`
/// supplies content hashes for files whose size and modification time did not
/// move, so an unchanged sysroot is not re-read on every reload.
pub fn digests(paths: &[String], previous: &[Digest]) -> Result<Vec<Digest>, String> {
    let known: HashMap<&str, &Digest> = previous.iter().map(|d| (d.path.as_str(), d)).collect();
    let mut files = Vec::new();
    for p in paths {
        collect(Path::new(p), &mut files)?;
    }
    files.sort();
    files.dedup();
    files
        .into_iter()
        .map(|path| {
            let meta = fs::metadata(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
            let size = meta.len();
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            if let Some(k) = known.get(path.as_str()) {
                if k.size == size && k.modified == modified {
                    return Ok((*k).clone());
                }
            }
            let bytes = fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            Ok(Digest {
                path,
                size,
                modified,
                content: h.finish(),
            })
        })
        .collect()
}

fn collect(path: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)
            .map_err(|e| format!("cannot list {}: {e}", path.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            collect(&e, out)?;
        }
    } else {
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(())
}

/// Inputs whose content differs, by path.
pub fn changed(old: &[Digest], new: &[Digest]) -> Vec<String> {
    let before: HashMap<&str, u64> = old.iter().map(|d| (d.path.as_str(), d.content)).collect();
    let after: HashMap<&str, u64> = new.iter().map(|d| (d.path.as_str(), d.content)).collect();
    let mut out: Vec<String> = before
        .iter()
        .filter(|(p, c)| after.get(*p) != Some(c))
        .map(|(p, _)| p.to_string())
        .chain(after.keys().filter(|p| !before.contains_key(*p)).map(|p| p.to_string()))
        .collect();
    out.sort();
    out
}

/// A content hash of the running patch builder's executable.
fn builder_identity() -> Result<u64, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the patch builder: {e}"))?;
    let bytes = fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Ok(h.finish())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn read_string(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}
