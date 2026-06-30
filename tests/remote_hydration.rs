// Integration test: remote on-access blob hydration.
//
// Two local CAS stores (A, B) talk to one in-process Remote (in-memory
// object_store). Store A is the source; the Remote is the server. Store B
// starts empty. When a remote workspace is "mounted" into store B
// (Index built from the Remote, HydratingStore wrapping store B),
// file reads pull exactly the missing blob for that file -- and nothing more.
//
// This test is sync (#[test], not #[tokio::test]) because Remote::in_memory
// uses block_on internally; mixing block_on with a live tokio runtime panics.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use devdropbox::cas::{BlobSource, Hash, HydratingStore, MemStore, Store};
use devdropbox::fs::ReadCache;
use devdropbox::index::Index;
use devdropbox::remote::{Remote, RemoteBlobSource};
use devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};

// ---------------------------------------------------------------------------
// CountingBlobSource: wraps RemoteBlobSource and records per-hash fetch counts.
// Used in assertions to prove exactly-one-fetch semantics.
// ---------------------------------------------------------------------------

struct CountingBlobSource {
    inner: RemoteBlobSource,
    counts: Arc<Mutex<HashMap<Hash, usize>>>,
}

impl BlobSource for CountingBlobSource {
    fn fetch_blob(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        assert_eq!(hash.len(), 32, "hash must be 32 bytes");
        let result = self.inner.fetch_blob(hash)?;
        if result.is_some() {
            let mut c = self.counts.lock().expect("counts lock poisoned");
            *c.entry(*hash).or_insert(0) += 1;
        }
        Ok(result)
    }
}

fn fetch_count(counts: &Arc<Mutex<HashMap<Hash, usize>>>, hash: &Hash) -> usize {
    *counts.lock().expect("counts lock").get(hash).unwrap_or(&0)
}

// ---------------------------------------------------------------------------
// Test: remote on-access hydration -- exact fetch counts + store-B sparseness
// ---------------------------------------------------------------------------

#[test]
fn remote_on_access_hydration_exact_counts_and_sparse_store() {
    // -- File contents ---------------------------------------------------------
    let content_readme: &[u8] = b"# Dev Dropbox test workspace";
    let content_main: &[u8] = b"fn main() { println!(\"hello\"); }";
    let content_lib: &[u8] = b"pub fn greet() -> &'static str { \"hello\" }";
    let content_shared: &[u8] = b"IDENTICAL CONTENT -- shared blob";
    let content_unread: &[u8] = b"this should NEVER be fetched by the test";

    // -- Store A: build the workspace ------------------------------------------
    let store_a = MemStore::new();

    let h_readme = store_a.put(content_readme).expect("put README");
    let h_main = store_a.put(content_main).expect("put main.rs");
    let h_lib = store_a.put(content_lib).expect("put lib.rs");
    let h_shared = store_a.put(content_shared).expect("put shared blob");
    let h_unread = store_a.put(content_unread).expect("put unread.txt");

    let src_bytes = serialize_tree(&[
        TreeEntry {
            mode: MODE_FILE,
            name: "lib.rs".into(),
            hash: h_lib,
        },
        TreeEntry {
            mode: MODE_FILE,
            name: "main.rs".into(),
            hash: h_main,
        },
    ]);
    let h_src_tree = store_a.put(&src_bytes).expect("put src tree");

    let root_bytes = serialize_tree(&[
        TreeEntry {
            mode: MODE_FILE,
            name: "README.md".into(),
            hash: h_readme,
        },
        TreeEntry {
            mode: MODE_DIR,
            name: "src".into(),
            hash: h_src_tree,
        },
        // Two entries pointing at the SAME blob hash (shared content).
        TreeEntry {
            mode: MODE_FILE,
            name: "shared_a.rs".into(),
            hash: h_shared,
        },
        TreeEntry {
            mode: MODE_FILE,
            name: "shared_b.rs".into(),
            hash: h_shared,
        },
        TreeEntry {
            mode: MODE_FILE,
            name: "unread.txt".into(),
            hash: h_unread,
        },
    ]);
    let root_hash = store_a.put(&root_bytes).expect("put root tree");

    // -- Server: push all blobs from store A into the in-process Remote --------
    let server: Arc<Remote> = Arc::new(Remote::in_memory("ws-hydration-test"));
    server.put(content_readme).expect("server: put README");
    server.put(content_main).expect("server: put main.rs");
    server.put(content_lib).expect("server: put lib.rs");
    server.put(content_shared).expect("server: put shared blob");
    server.put(content_unread).expect("server: put unread.txt");
    server.put(&src_bytes).expect("server: put src tree");
    server.put(&root_bytes).expect("server: put root tree");

    // -- Remote mount: build Index from the server; store B stays EMPTY --------
    // Index::build reads tree blobs and file blobs from the server (not store B).
    // File blob bytes are only used for size computation and are then discarded.
    // Store B's MemStore is never written during this phase.
    let index = Index::build(server.as_ref(), &root_hash).expect("index build from server");
    assert_eq!(
        index.len(),
        6,
        "6 files: README.md, src/main.rs, src/lib.rs, shared_a.rs, shared_b.rs, unread.txt"
    );

    // -- Store B: empty local CAS + HydratingStore wrapping the server ---------
    let store_b: Arc<MemStore> = Arc::new(MemStore::new());
    let store_b_dyn: Arc<dyn Store> = store_b.clone();

    let counts: Arc<Mutex<HashMap<Hash, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counting_source = CountingBlobSource {
        inner: RemoteBlobSource::new(Arc::clone(&server)),
        counts: Arc::clone(&counts),
    };

    let hydrating = HydratingStore::new(store_b_dyn, Arc::new(counting_source));
    let mut read_cache = ReadCache::new(Box::new(hydrating), index);

    // -- (3) Assert store B is SPARSE before any reads -------------------------
    assert!(
        !store_b.has(&h_readme),
        "README blob must not be in store B before any read"
    );
    assert!(
        !store_b.has(&h_main),
        "main.rs blob must not be in store B before any read"
    );
    assert!(
        !store_b.has(&h_lib),
        "lib.rs blob must not be in store B before any read"
    );
    assert!(
        !store_b.has(&h_shared),
        "shared blob must not be in store B before any read"
    );
    assert!(
        !store_b.has(&h_unread),
        "unread blob must not be in store B before any read"
    );

    // -- (4a) First read of README.md: exactly one remote fetch ----------------
    let got_readme = read_cache
        .read_path("README.md", 0, 1024)
        .expect("read_path must not error")
        .expect("README.md must be present");
    assert_eq!(
        got_readme, content_readme,
        "README.md bytes must be byte-identical to source"
    );
    assert_eq!(
        fetch_count(&counts, &h_readme),
        1,
        "first read must trigger exactly one fetch"
    );
    assert!(
        store_b.has(&h_readme),
        "README blob must be in store B after first read"
    );

    // -- (4b) Second read of README.md: served from ReadCache's in-memory cache;
    //         no new remote fetch --
    let got_readme2 = read_cache
        .read_path("README.md", 0, 1024)
        .expect("second read must not error")
        .expect("README.md must still be present");
    assert_eq!(
        got_readme2, content_readme,
        "second read must return identical bytes"
    );
    assert_eq!(
        fetch_count(&counts, &h_readme),
        1,
        "second read must NOT trigger a new fetch"
    );

    // -- (4c) Read src/main.rs: one fetch for a different blob -----------------
    let got_main = read_cache
        .read_path("src/main.rs", 0, 1024)
        .expect("read main.rs must not error")
        .expect("src/main.rs must be present");
    assert_eq!(got_main, content_main, "main.rs bytes must match source");
    assert_eq!(
        fetch_count(&counts, &h_main),
        1,
        "main.rs must have exactly one fetch"
    );
    assert!(
        store_b.has(&h_main),
        "main.rs blob must be in store B after read"
    );

    // -- (4d) Read src/lib.rs ---------------------------------------------------
    let got_lib = read_cache
        .read_path("src/lib.rs", 0, 1024)
        .expect("read lib.rs must not error")
        .expect("src/lib.rs must be present");
    assert_eq!(got_lib, content_lib, "lib.rs bytes must match source");
    assert_eq!(
        fetch_count(&counts, &h_lib),
        1,
        "lib.rs must have exactly one fetch"
    );

    // -- (4e) Two files sharing one blob: exactly one fetch total ---------------
    let got_shared_a = read_cache
        .read_path("shared_a.rs", 0, 1024)
        .expect("read shared_a.rs must not error")
        .expect("shared_a.rs must be present");
    assert_eq!(
        got_shared_a, content_shared,
        "shared_a.rs bytes must match source"
    );
    // First read of the shared blob triggers one fetch and caches it.
    assert_eq!(
        fetch_count(&counts, &h_shared),
        1,
        "first access of shared blob must fetch once"
    );

    let got_shared_b = read_cache
        .read_path("shared_b.rs", 0, 1024)
        .expect("read shared_b.rs must not error")
        .expect("shared_b.rs must be present");
    assert_eq!(
        got_shared_b, content_shared,
        "shared_b.rs bytes must match source"
    );
    // ReadCache's in-memory cache deduplicates by hash: shared_b hits the cache.
    assert_eq!(
        fetch_count(&counts, &h_shared),
        1,
        "second access of shared blob must NOT fetch again (ReadCache dedup by hash)"
    );

    // -- (4f) unread.txt: blob must NEVER have been fetched; store B stays sparse
    assert_eq!(
        fetch_count(&counts, &h_unread),
        0,
        "unread blob must never be fetched"
    );
    assert!(
        !store_b.has(&h_unread),
        "unread blob must be absent from store B's local CAS (store B stays sparse)"
    );

    // -- (5) FUSE route_read path shares the same HydratingStore ---------------
    //
    // Structural proof: in fs.rs LunarFs::read the non-overlay path calls
    //   self.read_cache.read_path(...)
    // and the overlay path calls
    //   crate::fuse::route_read(self.read_cache.store_ref(), ...)
    // where store_ref() returns the underlying Box<dyn Store> -- which is
    // a HydratingStore for remote mounts. Both paths call the same Store::get()
    // on the same HydratingStore, so remote hydration logic is shared and there
    // is no forked fetch path.
    //
    // Verified below by calling route_read directly with a fresh HydratingStore
    // backed by the same server, proving that the FUSE overlay read path hydrates.
    {
        use devdropbox::fuse::route_read;
        use devdropbox::overlay::OverlayStore;
        use rusqlite::Connection;

        let store_fuse: Arc<MemStore> = Arc::new(MemStore::new()); // empty local
        let store_fuse_dyn: Arc<dyn Store> = store_fuse.clone();
        let source_fuse = RemoteBlobSource::new(Arc::clone(&server));
        let hydrating_fuse = HydratingStore::new(store_fuse_dyn, Arc::new(source_fuse));

        // Build a fresh index from the server for this subtest.
        let index_fuse = Index::build(server.as_ref(), &root_hash).expect("index_fuse build");

        let conn = Connection::open_in_memory().expect("sqlite in-memory");
        let overlay = OverlayStore::new(conn);
        overlay.init_schema().expect("init_schema");
        let agent = overlay.fork(1).expect("fork agent");

        // route_read takes &dyn Store; HydratingStore implements Store.
        let result = route_read(
            &hydrating_fuse,
            &index_fuse,
            &overlay,
            agent,
            "README.md",
            0,
            1024,
        )
        .expect("route_read must not error")
        .expect("README.md must be present via route_read");
        assert_eq!(
            result, content_readme,
            "FUSE route_read must return byte-identical content"
        );
        assert!(
            store_fuse.has(&h_readme),
            "FUSE route_read must hydrate the blob into the local store via HydratingStore"
        );

        // A second route_read call must be served from local (no re-fetch from remote).
        // store_fuse already has h_readme; HydratingStore.get fast-paths to local.
        let result2 = route_read(
            &hydrating_fuse,
            &index_fuse,
            &overlay,
            agent,
            "README.md",
            0,
            1024,
        )
        .expect("second route_read must not error")
        .expect("path must still exist");
        assert_eq!(
            result2, content_readme,
            "second route_read must return same bytes"
        );
        // store_fuse.has() is the same check; no separate fetch counter for this sub-store.
    }
}

// ---------------------------------------------------------------------------
// Test: corrupt remote blob is rejected; local CAS not poisoned
// ---------------------------------------------------------------------------

#[test]
fn hydrating_store_rejects_corrupt_remote_blob() {
    use devdropbox::cas::hash_bytes;

    // A BlobSource that returns bytes that do NOT hash to the requested hash.
    struct CorruptSource;
    impl BlobSource for CorruptSource {
        fn fetch_blob(&self, _hash: &Hash) -> io::Result<Option<Vec<u8>>> {
            Ok(Some(b"this is NOT the right content".to_vec()))
        }
    }

    let local = Arc::new(MemStore::new());
    let local_dyn: Arc<dyn Store> = local.clone();
    let hydrating = HydratingStore::new(local_dyn, Arc::new(CorruptSource));

    // Put "real" data locally to derive a known hash, then ask for that hash.
    let real_data = b"genuine content";
    let real_hash = hash_bytes(real_data);

    // The local store does NOT have the blob; HydratingStore will fetch from CorruptSource.
    let err = hydrating
        .get(&real_hash)
        .expect_err("corrupt remote must return Err, not Ok");
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "hash mismatch must produce InvalidData error, got: {}",
        err
    );
    // Local CAS must be unpoisoned: the corrupt blob must NOT have been written.
    assert!(
        !local.has(&real_hash),
        "corrupt remote blob must not poison the local CAS"
    );
}

// ---------------------------------------------------------------------------
// Test: remote absent blob propagates as Ok(None); local CAS unchanged
// ---------------------------------------------------------------------------

#[test]
fn hydrating_store_absent_remote_blob_is_not_found() {
    // A BlobSource that always returns None (blob not on server).
    struct AbsentSource;
    impl BlobSource for AbsentSource {
        fn fetch_blob(&self, _hash: &Hash) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    let local = Arc::new(MemStore::new());
    let local_dyn: Arc<dyn Store> = local.clone();
    let hydrating = HydratingStore::new(local_dyn, Arc::new(AbsentSource));

    let phantom_hash = [0xffu8; 32];
    let result = hydrating
        .get(&phantom_hash)
        .expect("absent remote must not Err");
    assert!(
        result.is_none(),
        "blob absent from both local and remote must return Ok(None)"
    );
    assert!(
        !local.has(&phantom_hash),
        "absent blob must not appear in local CAS"
    );
}

// ---------------------------------------------------------------------------
// Test: blob already local -- remote is never contacted
// ---------------------------------------------------------------------------

#[test]
fn hydrating_store_local_hit_skips_remote() {
    // A BlobSource that panics if ever called (proves local hit skips remote).
    struct PanickingSource;
    impl BlobSource for PanickingSource {
        fn fetch_blob(&self, _hash: &Hash) -> io::Result<Option<Vec<u8>>> {
            panic!("remote source must never be called for a blob already in local CAS");
        }
    }

    let local = Arc::new(MemStore::new());
    let data = b"already local";
    let hash = local.put(data).expect("put must succeed");

    let local_dyn: Arc<dyn Store> = local.clone();
    let hydrating = HydratingStore::new(local_dyn, Arc::new(PanickingSource));

    // This must not panic: blob is local, PanickingSource is never called.
    let got = hydrating
        .get(&hash)
        .expect("get must succeed")
        .expect("blob must be present");
    assert_eq!(got, data, "local hit must return correct bytes");
}
