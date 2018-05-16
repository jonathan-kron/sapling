// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! In memory manifests, used to convert Bonsai Changesets to old style

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use futures::future::{self, Future, IntoFuture};
use futures::stream::Stream;
use futures_ext::{BoxFuture, FutureExt};

use slog::Logger;

use mercurial::{HgNodeHash, NodeHashConversion};
use mercurial_types::{DManifestId, DParents, Entry, MPath, MPathElement, Manifest, RepoPath, Type};

use blobstore::Blobstore;
use file::HgBlobEntry;
use repo::{UploadHgEntry, UploadHgNodeHash};

use errors::*;
use manifest::BlobManifest;

/// An in-memory manifest entry
enum MemoryManifestEntry {
    /// A blob already found in the blob store. This cannot be a Tree blob
    Blob(HgBlobEntry),
    /// There are conflicting options here, to be resolved
    /// The vector contains each of the conflicting manifest entries, for use in generating
    /// parents of the final entry when bonsai changeset resolution removes this conflict
    Conflict(Vec<MemoryManifestEntry>),
    /// This entry is an in-memory tree, and will need writing out to finish
    /// resolving the manifests
    MemTree {
        children: BTreeMap<MPathElement, MemoryManifestEntry>,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
        modified: bool,
    },
}

// This is tied to the implementation of MemoryManifestEntry::save below
fn extend_repopath_with_dir(path: &RepoPath, dir: &MPathElement) -> RepoPath {
    assert!(path.is_dir() || path.is_root(), "Cannot extend a filepath");

    let opt_mpath = MPath::join_opt(path.mpath(), dir);
    match opt_mpath {
        None => RepoPath::root(),
        Some(p) => RepoPath::dir(p).expect("Can't convert an MPath to an MPath?!?"),
    }
}

impl MemoryManifestEntry {
    /// Save all manifests represented here to the blobstore
    pub fn save(
        &self,
        blobstore: &Arc<Blobstore>,
        logger: &Logger,
        path: RepoPath,
    ) -> BoxFuture<HgBlobEntry, Error> {
        match self {
            &MemoryManifestEntry::Blob(ref blob) => future::ok(blob.clone()).boxify(),
            &MemoryManifestEntry::Conflict(_) => {
                future::err(ErrorKind::UnresolvedConflicts.into()).boxify()
            }
            &MemoryManifestEntry::MemTree {
                ref children,
                p1,
                p2,
                modified,
            } => {
                if modified {
                    // Two things to do:
                    // 1: join_all() the recursive serialization of all entries
                    // 2: Write out a manifest and return its hash.
                    let futures: Vec<_> = children
                        .iter()
                        .map({
                            let blobstore = blobstore.clone();
                            let path = &path;
                            move |(path_elem, entry)| {
                                let path_elem = path_elem.clone();
                                // This is safe, because we only save if we're saving a tree
                                let entry_path = extend_repopath_with_dir(path, &path_elem);
                                entry
                                    .save(&blobstore, logger, entry_path)
                                    .map(move |entry| (path_elem, entry))
                            }
                        })
                        .collect();

                    let entries = future::join_all(futures.into_iter());

                    entries
                        .and_then({
                            let blobstore = blobstore.clone();
                            let logger = logger.clone();
                            move |entries| {
                                let mut manifest: Vec<u8> = Vec::new();
                                entries.iter().for_each(|&(ref path, ref entry)| {
                                    manifest.extend(path.as_bytes());
                                    // Chances of write to memory failing are low enough that this
                                    // should be safe to ignore
                                    let _ = write!(
                                        &mut manifest,
                                        "\0{}{}\n",
                                        entry.get_hash().into_nodehash(),
                                        entry.get_type(),
                                    );
                                });

                                let upload_entry = UploadHgEntry {
                                    upload_nodeid: UploadHgNodeHash::Generate,
                                    raw_content: manifest.into(),
                                    content_type: Type::Tree,
                                    p1,
                                    p2,
                                    path,
                                };

                                let (_hash, future) = try_boxfuture!(
                                    upload_entry.upload_to_blobstore(&blobstore, &logger)
                                );
                                future.map(|(entry, _path)| entry).boxify()
                            }
                        })
                        .boxify()
                } else {
                    // Not modified, so p2 must be None, p1 is manifest
                    // If this is not true, then we had either a merge that we need to record (p2
                    // not None), or we didn't have a manifest to base this on (p1 None) and yet
                    // we neither filled in a new entry at this level, nor left this empty.
                    // The only way we can end up in this situation is a serious programming error
                    assert!(
                        p2.is_none(),
                        "Modified bit not set correctly on in-memory manifest"
                    );
                    future::result(p1.ok_or(ErrorKind::UnchangedManifest.into()))
                        .and_then({
                            let blobstore = blobstore.clone();
                            move |p1| {
                                HgBlobEntry::new(
                                    blobstore,
                                    path.mpath().map(MPath::basename).cloned(),
                                    p1.into_mononoke(),
                                    Type::Tree,
                                ).into_future()
                            }
                        })
                        .boxify()
                }
            }
        }
    }

    /// Create a MemoryManifestEntry from an existing Mercurial tree.
    pub fn convert_treenode(
        blobstore: Arc<Blobstore>,
        manifest_id: &DManifestId,
    ) -> BoxFuture<Self, Error> {
        // This reads in the manifest, keeps it as p1, and converts it to a memory manifest node
        BlobManifest::load(&blobstore, manifest_id)
            .and_then({
                let manifest_id = manifest_id.clone();
                move |m| {
                    future::result(m.ok_or(
                        ErrorKind::ManifestMissing(manifest_id.into_nodehash()).into(),
                    ))
                }
            })
            .and_then({
                let blobstore = blobstore.clone();
                move |m| {
                    m.list()
                        .and_then(move |entry| {
                            let name = entry
                                .get_name()
                                .expect("Unnamed entry in a manifest")
                                .clone();
                            match entry.get_type() {
                                Type::Tree => Self::convert_treenode(
                                    blobstore.clone(),
                                    &DManifestId::new(entry.get_hash().into_nodehash()),
                                ).map(move |entry| (name, entry))
                                    .boxify(),
                                _ => future::ok(MemoryManifestEntry::Blob(try_boxfuture!(
                                    HgBlobEntry::new(
                                        blobstore.clone(),
                                        Some(name.clone()),
                                        entry.get_hash().into_nodehash(),
                                        entry.get_type(),
                                    )
                                ))).map(move |entry| (name, entry))
                                    .boxify(),
                            }
                        })
                        .fold(BTreeMap::new(), move |mut children, (name, entry)| {
                            children.insert(name, entry);
                            future::ok::<_, Error>(children)
                        })
                }
            })
            .map({
                let manifest_id = manifest_id.clone();
                move |children| MemoryManifestEntry::MemTree {
                    children,
                    p1: Some(manifest_id.into_nodehash().into_mercurial()),
                    p2: None,
                    modified: false,
                }
            })
            .boxify()
    }

    /// True if this entry is a Tree, false otherwise
    #[cfg(test)]
    pub fn is_dir(&self) -> bool {
        match self {
            &MemoryManifestEntry::MemTree { .. } => true,
            _ => false,
        }
    }
}

/// An in memory manifest, created from parent manifests (if any)
pub struct MemoryRootManifest {
    blobstore: Arc<Blobstore>,
    root_entry: MemoryManifestEntry,
}

impl MemoryRootManifest {
    fn create(blobstore: Arc<Blobstore>, root_entry: MemoryManifestEntry) -> Self {
        Self {
            blobstore,
            root_entry,
        }
    }

    /// Create an in-memory manifest, backed by the given blobstore, and based on mp1 and mp2
    pub fn new(
        blobstore: Arc<Blobstore>,
        mp1: Option<&HgNodeHash>,
        mp2: Option<&HgNodeHash>,
    ) -> BoxFuture<Self, Error> {
        let parents = DParents::new(
            mp1.map(|p| p.into_mononoke()).as_ref(),
            mp2.map(|p| p.into_mononoke()).as_ref(),
        );
        match parents {
            DParents::None => future::ok(Self::create(
                blobstore,
                MemoryManifestEntry::MemTree {
                    children: BTreeMap::new(),
                    p1: None,
                    p2: None,
                    modified: false,
                },
            )).boxify(),
            DParents::One(p) => {
                MemoryManifestEntry::convert_treenode(blobstore.clone(), &DManifestId::new(p))
                    .map(move |root_entry| Self::create(blobstore, root_entry))
                    .boxify()
            }
            // TODO: This is where the merge case ends up going, when I've worked out
            // what it looks like. For now, it's all conflicting
            DParents::Two(p1, p2) => {
                let p1_conflict =
                    MemoryManifestEntry::convert_treenode(blobstore.clone(), &DManifestId::new(p1));
                let p2_conflict =
                    MemoryManifestEntry::convert_treenode(blobstore.clone(), &DManifestId::new(p2));
                p1_conflict
                    .join(p2_conflict)
                    .map(|conflicts| {
                        Self::create(
                            blobstore,
                            MemoryManifestEntry::Conflict(vec![conflicts.0, conflicts.1]),
                        )
                    })
                    .boxify()
            }
        }
    }

    /// Save this manifest to the blobstore, recursing down to ensure that
    /// all child entries are saved and that there are no conflicts.
    /// Note that child entries can be saved even if a parallel tree has conflicts. E.g. if the
    /// manifest contains dir1/file1 and dir2/file2 and dir2 contains a conflict for file2, dir1
    /// can still be saved to the blobstore.
    /// Returns the saved manifest ID
    pub fn save(self, logger: &Logger) -> BoxFuture<HgBlobEntry, Error> {
        self.root_entry
            .save(&self.blobstore, logger, RepoPath::root())
            .boxify()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_unit;
    use many_files_dirs;
    use mercurial_types::DNodeHash;
    use slog::Discard;

    fn insert_entry(
        tree: &mut MemoryManifestEntry,
        path: MPathElement,
        entry: MemoryManifestEntry,
    ) {
        match tree {
            &mut MemoryManifestEntry::MemTree {
                ref mut children,
                ref mut modified,
                ..
            } => {
                *modified = true;
                children.insert(path, entry);
            }
            _ => panic!("Inserting into a non-Tree"),
        }
    }

    #[test]
    fn empty_manifest() {
        async_unit::tokio_unit_test(|| {
            let blobstore = many_files_dirs::getrepo(None).get_blobstore();

            // Create an empty memory manifest
            let memory_manifest = MemoryRootManifest::new(blobstore, None, None)
                .wait()
                .expect("Could not create empty manifest");

            if let MemoryManifestEntry::MemTree {
                children,
                p1,
                p2,
                modified,
            } = memory_manifest.root_entry
            {
                assert!(children.is_empty(), "Empty manifest had children");
                assert!(p1.is_none(), "Empty manifest had p1");
                assert!(p2.is_none(), "Empty manifest had p2");
                assert!(!modified, "Empty (unaltered) manifest is modified");
            } else {
                panic!("Empty manifest is not a MemTree");
            }
        })
    }

    #[test]
    fn load_manifest() {
        async_unit::tokio_unit_test(|| {
            let blobstore = many_files_dirs::getrepo(None).get_blobstore();

            let manifest_id = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash")
                .into_mercurial();

            // Load a memory manifest
            let memory_manifest = MemoryRootManifest::new(blobstore, Some(&manifest_id), None)
                .wait()
                .expect("Could not load manifest");

            if let MemoryManifestEntry::MemTree {
                children,
                p1,
                p2,
                modified,
            } = memory_manifest.root_entry
            {
                for (path, entry) in children {
                    match path.as_bytes() {
                        b"1" | b"2" | b"dir1" => {
                            assert!(!entry.is_dir(), "{:?} is not a file", path)
                        }
                        b"dir2" => assert!(entry.is_dir(), "{:?} is not a tree", path),
                        _ => panic!("Unknown path {:?}", path),
                    }
                }
                assert!(
                    p1 == Some(manifest_id),
                    "Loaded manifest had wrong p1 {:?}",
                    p1
                );
                assert!(p2.is_none(), "Loaded manifest had p2");
                assert!(!modified, "Loaded (unaltered) manifest is modified");
            } else {
                panic!("Loaded manifest is not a MemTree");
            }
        })
    }

    #[test]
    fn save_manifest() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            // Create an empty memory manifest
            let mut memory_manifest = MemoryRootManifest::new(blobstore, None, None)
                .wait()
                .expect("Could not create empty manifest");

            // Add an entry
            let dir_nodehash = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash");
            let dir = MemoryManifestEntry::MemTree {
                children: BTreeMap::new(),
                p1: Some(dir_nodehash.into_mercurial()),
                p2: None,
                modified: false,
            };
            let path =
                MPathElement::new(b"dir".to_vec()).expect("dir is no longer a valid MPathElement");
            insert_entry(&mut memory_manifest.root_entry, path.clone(), dir);

            let manifest_id = memory_manifest
                .save(&logger)
                .wait()
                .expect("Could not save manifest");

            let refound = repo.get_manifest_by_nodeid(&manifest_id.get_hash().into_nodehash())
                .and_then(|m| m.lookup(&path))
                .wait()
                .expect("Lookup of entry just saved failed")
                .expect("Just saved entry not present");

            assert_eq!(
                refound.get_hash().into_nodehash(),
                dir_nodehash,
                "directory hash changed"
            );
        })
    }
}
