//! In-memory inode table built from a REAPI `Directory` Merkle DAG.
//!
//! The FUSE filesystem (`super::mount`) serves `getattr` / `lookup` /
//! `readdir` / `readlink` directly out of this table — no CAS round-trip
//! for any directory-level metadata. Only the file *content* is fetched
//! lazily on the first `read(2)`.
//!
//! Building the table is the only async step in the mount path: it
//! walks the Merkle DAG (one CAS `get` per `Directory` proto). For a
//! tree with N subdirectories the walk does N CAS calls, all for
//! proto-sized blobs. File content blobs are *not* fetched here.
//!
//! This module is platform-independent on purpose so it can be unit
//! tested without `/dev/fuse`. Anything that touches `fuser` belongs in
//! [`super::mount`].

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use brokkr_cas::traits::Cas;
use brokkr_cas::CasError;
use brokkr_common::Digest;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;

/// First user-assigned inode. `1` is reserved by the kernel for the
/// FUSE root (matches `fuser::FUSE_ROOT_ID`). We hand out `2, 3, …`
/// for everything else.
pub const ROOT_INO: u64 = 1;

/// One filesystem entry, indexed by inode number in [`InodeTable`].
#[derive(Debug, Clone)]
pub struct Inode {
    /// Inode number. Equal to the entry's index in the table + 1.
    pub ino: u64,
    /// What kind of entry this is + the data needed to serve it.
    pub kind: InodeKind,
}

/// File / directory / symlink discriminant for [`Inode`].
#[derive(Debug, Clone)]
pub enum InodeKind {
    /// Directory: child entries by name.
    Dir {
        /// Name → child inode. We use a `HashMap` so `lookup` is O(1)
        /// even for directories with thousands of entries (Bazel's
        /// `external/` is a famous example).
        entries: HashMap<OsString, u64>,
    },
    /// Regular file: CAS digest of the content, plus the metadata
    /// `getattr` needs without touching CAS.
    File {
        /// Content digest. Resolved lazily on first `read(2)`.
        digest: Digest,
        /// Size in bytes; reported via `getattr` so the kernel can do
        /// short reads correctly without us fetching first.
        size: u64,
        /// Whether the REAPI `FileNode.is_executable` bit was set.
        /// Translated to mode `0o555` vs `0o444` at `getattr` time.
        exec: bool,
    },
    /// Symlink. Target string is returned verbatim per REAPI v2
    /// §SymlinkNode — no canonicalisation, may be absolute or
    /// relative.
    Link {
        /// Symlink target, as stored in the REAPI proto.
        target: OsString,
    },
}

/// Full inode set for one mounted input tree.
///
/// Built once by [`InodeTable::build`]; immutable afterwards.
#[derive(Debug, Clone)]
pub struct InodeTable {
    inodes: Vec<Inode>,
}

impl InodeTable {
    /// Walk the `Directory` Merkle DAG rooted at `root_digest` and
    /// return the populated table. Performs one CAS `get` per
    /// `Directory` proto encountered; does **not** fetch file
    /// content.
    pub async fn build(cas: &dyn Cas, root_digest: &Digest) -> Result<Self, CasError> {
        let mut inodes: Vec<Inode> = Vec::new();
        // Reserve the root slot up front so its inode is ROOT_INO.
        inodes.push(Inode {
            ino: ROOT_INO,
            kind: InodeKind::Dir {
                entries: HashMap::new(),
            },
        });
        build_dir(cas, root_digest, ROOT_INO, &mut inodes).await?;
        Ok(InodeTable { inodes })
    }

    /// Root directory inode number (always [`ROOT_INO`]).
    pub fn root_ino(&self) -> u64 {
        ROOT_INO
    }

    /// Look up an inode by number. Returns `None` for unknown
    /// inodes (kernel may send stale numbers after a mount restart).
    pub fn get(&self, ino: u64) -> Option<&Inode> {
        if ino == 0 {
            return None;
        }
        self.inodes.get((ino - 1) as usize)
    }

    /// Resolve `parent_ino` + `name` to the child inode, if any.
    /// Returns `None` if the parent is not a directory or the name
    /// is absent.
    pub fn lookup(&self, parent_ino: u64, name: &OsStr) -> Option<u64> {
        match &self.get(parent_ino)?.kind {
            InodeKind::Dir { entries } => entries.get(name).copied(),
            _ => None,
        }
    }

    /// Iterate the children of a directory inode. Order is
    /// unspecified (HashMap iteration); callers that need stable
    /// ordering should sort.
    pub fn children(&self, dir_ino: u64) -> Option<impl Iterator<Item = (&OsStr, u64)> + '_> {
        match &self.get(dir_ino)?.kind {
            InodeKind::Dir { entries } => {
                Some(entries.iter().map(|(name, ino)| (name.as_os_str(), *ino)))
            }
            _ => None,
        }
    }

    /// Total inode count — useful for diagnostics and tests.
    pub fn len(&self) -> usize {
        self.inodes.len()
    }

    /// True iff the table has no entries (impossible after a
    /// successful `build`, which always inserts the root).
    pub fn is_empty(&self) -> bool {
        self.inodes.is_empty()
    }

    /// Resolve a `/`-rooted path to an inode. Test helper; FUSE
    /// itself uses [`Self::lookup`] one component at a time.
    pub fn resolve(&self, path: &Path) -> Option<u64> {
        let mut ino = self.root_ino();
        for component in path.components() {
            use std::path::Component;
            match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    ino = self.lookup(ino, name)?;
                }
                Component::ParentDir | Component::Prefix(_) => return None,
            }
        }
        Some(ino)
    }
}

// Recursive DAG walker. Boxed because async fn can't recurse without
// it on stable; the alternative is a work-stack. Tree depth is
// bounded by the action's input tree (Bazel-style: shallow, <20).
fn build_dir<'a>(
    cas: &'a dyn Cas,
    digest: &'a Digest,
    parent_ino: u64,
    inodes: &'a mut Vec<Inode>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CasError>> + Send + 'a>> {
    Box::pin(async move {
        let directory = fetch_directory(cas, digest).await?;

        for file_node in &directory.files {
            let file_digest = decode_digest(&file_node.digest, &file_node.name, "FileNode")?;
            let size = u64::try_from(file_digest.size_bytes()).map_err(|_| {
                CasError::Other(format!(
                    "FileNode {:?}: negative or oversize digest size {}",
                    file_node.name,
                    file_digest.size_bytes()
                ))
            })?;
            let child_ino = push(
                inodes,
                InodeKind::File {
                    digest: file_digest,
                    size,
                    exec: file_node.is_executable,
                },
            );
            link_child(inodes, parent_ino, &file_node.name, child_ino)?;
        }

        for symlink in &directory.symlinks {
            let child_ino = push(
                inodes,
                InodeKind::Link {
                    target: OsString::from(&symlink.target),
                },
            );
            link_child(inodes, parent_ino, &symlink.name, child_ino)?;
        }

        for child in &directory.directories {
            let child_digest = decode_digest(&child.digest, &child.name, "DirectoryNode")?;
            let child_ino = push(
                inodes,
                InodeKind::Dir {
                    entries: HashMap::new(),
                },
            );
            link_child(inodes, parent_ino, &child.name, child_ino)?;
            build_dir(cas, &child_digest, child_ino, inodes).await?;
        }

        Ok(())
    })
}

fn push(inodes: &mut Vec<Inode>, kind: InodeKind) -> u64 {
    let ino = (inodes.len() as u64) + 1;
    inodes.push(Inode { ino, kind });
    ino
}

fn link_child(
    inodes: &mut [Inode],
    parent_ino: u64,
    name: &str,
    child_ino: u64,
) -> Result<(), CasError> {
    let idx = (parent_ino - 1) as usize;
    let parent = inodes
        .get_mut(idx)
        .ok_or_else(|| CasError::Other(format!("parent inode {parent_ino} not allocated")))?;
    match &mut parent.kind {
        InodeKind::Dir { entries } => {
            if entries.insert(OsString::from(name), child_ino).is_some() {
                return Err(CasError::Other(format!(
                    "duplicate entry {name:?} in inode {parent_ino}"
                )));
            }
            Ok(())
        }
        _ => Err(CasError::Other(format!(
            "parent inode {parent_ino} is not a directory"
        ))),
    }
}

fn decode_digest(
    proto: &Option<rapi::Digest>,
    name: &str,
    kind: &'static str,
) -> Result<Digest, CasError> {
    match proto {
        Some(d) => Digest::new(d.hash.clone(), d.size_bytes)
            .map_err(|e| CasError::Other(format!("invalid {kind} digest {name}: {e}"))),
        None => Err(CasError::Other(format!("{kind} {name:?} missing digest"))),
    }
}

async fn fetch_directory(cas: &dyn Cas, digest: &Digest) -> Result<rapi::Directory, CasError> {
    let mut results = cas.batch_read_blobs(&[digest.clone()]).await?;
    let bytes = match results.pop() {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(e),
        None => return Err(CasError::NotFound(digest.clone())),
    };
    rapi::Directory::decode(bytes.as_ref())
        .map_err(|e| CasError::Other(format!("Directory decode for {digest:?}: {e}")))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]
mod tests {
    use super::*;
    use brokkr_cas::tree::build_tree_into;
    use brokkr_cas::InMemoryCas;
    use std::path::PathBuf;

    async fn build_from_disk(src: &Path) -> (InodeTable, InMemoryCas) {
        let cas = InMemoryCas::new();
        let root = build_tree_into(&cas, src).await.unwrap();
        let table = InodeTable::build(&cas, &root).await.unwrap();
        (table, cas)
    }

    #[tokio::test]
    async fn empty_tree_has_only_root() {
        let src = tempfile::tempdir().unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;
        assert_eq!(table.len(), 1);
        assert_eq!(table.root_ino(), ROOT_INO);
        let root = table.get(ROOT_INO).unwrap();
        match &root.kind {
            InodeKind::Dir { entries } => assert!(entries.is_empty()),
            _ => panic!("root must be a directory"),
        }
    }

    #[tokio::test]
    async fn flat_files_assigned_distinct_inodes() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("b.bin"), b"\x00\x01\x02").unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;

        assert_eq!(table.len(), 3); // root + 2 files
        let a = table.resolve(&PathBuf::from("/a.txt")).unwrap();
        let b = table.resolve(&PathBuf::from("/b.bin")).unwrap();
        assert_ne!(a, b);

        match &table.get(a).unwrap().kind {
            InodeKind::File { size, exec, .. } => {
                assert_eq!(*size, 5);
                assert!(!exec);
            }
            other => panic!("a.txt was {other:?}"),
        }
        match &table.get(b).unwrap().kind {
            InodeKind::File { size, .. } => assert_eq!(*size, 3),
            other => panic!("b.bin was {other:?}"),
        }
    }

    #[tokio::test]
    async fn nested_tree_resolves_paths() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("top.txt"), b"top").unwrap();
        let sub = src.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), b"inner").unwrap();
        let deeper = sub.join("deeper");
        std::fs::create_dir(&deeper).unwrap();
        std::fs::write(deeper.join("bottom.txt"), b"bottom").unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;

        // root, top.txt, sub, sub/inner.txt, sub/deeper, sub/deeper/bottom.txt
        assert_eq!(table.len(), 6);

        let bottom = table
            .resolve(&PathBuf::from("/sub/deeper/bottom.txt"))
            .unwrap();
        match &table.get(bottom).unwrap().kind {
            InodeKind::File { size, .. } => assert_eq!(*size, 6),
            other => panic!("bottom was {other:?}"),
        }
        assert!(table.resolve(&PathBuf::from("/sub/missing")).is_none());
    }

    #[tokio::test]
    async fn executable_bit_propagates_to_inode() {
        use std::os::unix::fs::PermissionsExt as _;
        let src = tempfile::tempdir().unwrap();
        let script = src.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;

        let ino = table.resolve(&PathBuf::from("/run.sh")).unwrap();
        match &table.get(ino).unwrap().kind {
            InodeKind::File { exec, .. } => assert!(*exec),
            other => panic!("run.sh was {other:?}"),
        }
    }

    #[tokio::test]
    async fn symlink_target_preserved_verbatim() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("target.txt"), b"linked").unwrap();
        std::os::unix::fs::symlink("target.txt", src.path().join("link")).unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;

        let link = table.resolve(&PathBuf::from("/link")).unwrap();
        match &table.get(link).unwrap().kind {
            InodeKind::Link { target } => assert_eq!(target.as_os_str(), "target.txt"),
            other => panic!("link was {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_root_propagates_not_found() {
        let cas = InMemoryCas::new();
        let bogus = Digest::of(b"not in cas");
        let err = InodeTable::build(&cas, &bogus).await.unwrap_err();
        assert!(matches!(err, CasError::NotFound(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn lookup_returns_none_for_non_directory_parent() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"x").unwrap();
        let (table, _cas) = build_from_disk(src.path()).await;
        let a = table.resolve(&PathBuf::from("/a.txt")).unwrap();
        assert!(table.lookup(a, OsStr::new("anything")).is_none());
    }
}
