/// Smoke test: presign path against real Cloudflare R2.
/// Skipped unless LUNAR_SMOKE=1 is set in the environment AND all
/// required R2 credentials are present.
/// Run via: scripts/smoke-r2-presign.sh
use devdropbox::cas::{hash_to_hex, Hash, MemStore, Store};
use devdropbox::index::Index;
use devdropbox::remote::HttpRemote;
use devdropbox::sync::{pull, push};
use devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
use object_store::aws::AmazonS3Builder;
use object_store::{path::Path as ObjPath, ObjectStore};
use std::sync::Arc;

const SMOKE_TOKEN: &str = "smoke-r2-presign-token";

// --- RAII guard: kill + reap child on drop so a panicking test never leaks ---

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// --- Fixture: 3 files in a nested tree ---

struct Fixture {
    store: MemStore,
    root: Hash,
    all_hashes: Vec<Hash>,
}

fn build_fixture() -> Fixture {
    let store = MemStore::new();
    let h_readme = store.put(b"# Dev Dropbox presign smoke\n").expect("put README");
    let h_main = store.put(b"fn main() { println!(\"presign\"); }\n").expect("put main.rs");
    let h_lib = store.put(b"pub fn greet() -> &'static str { \"presign\" }\n").expect("put lib.rs");

    let src_bytes = serialize_tree(&[
        TreeEntry { mode: MODE_FILE, name: "lib.rs".into(), hash: h_lib },
        TreeEntry { mode: MODE_FILE, name: "main.rs".into(), hash: h_main },
    ]);
    let h_src = store.put(&src_bytes).expect("put src tree");

    let root_bytes = serialize_tree(&[
        TreeEntry { mode: MODE_FILE, name: "README.md".into(), hash: h_readme },
        TreeEntry { mode: MODE_DIR, name: "src".into(), hash: h_src },
    ]);
    let root = store.put(&root_bytes).expect("put root tree");

    let all_hashes = vec![h_readme, h_main, h_lib, h_src, root];
    Fixture { store, root, all_hashes }
}

#[tokio::test]
async fn smoke_r2_presign_roundtrip() {
    // --- GATE: skip cleanly when flag or any required credential is absent ---

    if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
        eprintln!("smoke_r2_presign: skip (LUNAR_SMOKE is not set to 1)");
        return;
    }

    macro_rules! require_env {
        ($name:expr) => {
            match std::env::var($name) {
                Ok(v) if !v.is_empty() => v,
                _ => {
                    eprintln!("smoke_r2_presign: skip ({} is missing or empty)", $name);
                    return;
                }
            }
        };
    }

    let bucket = require_env!("LUNAR_REMOTE_BUCKET");
    let endpoint = require_env!("LUNAR_REMOTE_ENDPOINT");
    let account_id = require_env!("LUNAR_R2_ACCOUNT_ID");
    let access_key = require_env!("AWS_ACCESS_KEY_ID");
    let secret_key = require_env!("AWS_SECRET_ACCESS_KEY");
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "auto".to_string());

    eprintln!(
        "smoke_r2_presign: bucket={} endpoint={} account_id={} region={}",
        bucket, endpoint, account_id, region
    );

    // --- PORT: bind ephemeral, read assigned port, drop so the child can bind ---

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    let addr = format!("127.0.0.1:{}", port);
    let base_url = format!("http://{}", addr);
    eprintln!("smoke_r2_presign: serving on {}", addr);

    // --- BOOT REAL BINARY (with R2 store, so server builds an R2Presigner) ---

    let bin = std::env::var("CARGO_BIN_EXE_lunar")
        .expect("CARGO_BIN_EXE_lunar must be set by the cargo test harness");

    let child = std::process::Command::new(&bin)
        .args(["serve", "--store", &format!("s3://{}", bucket), "--addr", &addr])
        .env("AWS_ACCESS_KEY_ID", &access_key)
        .env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .env("AWS_ENDPOINT", &endpoint)
        .env("AWS_REGION", &region)
        .env("LUNAR_TOKENS", SMOKE_TOKEN)
        .spawn()
        .expect("spawn lunar binary");

    let _guard = ChildGuard(child);

    // --- READINESS PROBE ---

    let http_client = reqwest::Client::new();
    let probe_url = format!("{}/v1/ref/smoke-presign-absent", base_url);
    let mut ready = false;
    for _ in 0..40usize {
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
        if http_client.get(&probe_url).bearer_auth(SMOKE_TOKEN).send().await.is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "lunar serve did not become ready within ~6s on {}", addr);
    eprintln!("smoke_r2_presign: server is ready");

    // --- WORKSPACE ---

    let workspace = format!("smoke-r2-presign-{}", std::process::id());
    eprintln!("smoke_r2_presign: workspace={}", workspace);

    // --- EXPLICIT PRESIGN CHECK: verify the server mints an https:// URL for R2 ---

    let remote = HttpRemote::new(&base_url, SMOKE_TOKEN);
    let fixture = build_fixture();

    // Spot-check: presign the root hash as a GET and verify the URL is an https:// URL.
    let presigned =
        remote.presign(&fixture.root, devdropbox::presign::PresignOp::Get, Some(&workspace))
        .await
        .expect("R2 presign must succeed");
    assert!(
        presigned.url.starts_with("https://"),
        "R2 presigned URL must be an https:// URL, got: {}",
        presigned.url
    );
    assert_eq!(presigned.method, "GET", "presign GET must return method GET");
    assert!(presigned.expires_at > 0, "expires_at must be positive");
    eprintln!("smoke_r2_presign: presigned URL verified (https schema)");

    // --- PUSH via presign path (HttpRemote::put_blob uses presign-first) ---

    let upload_count = push(&fixture.store, &fixture.root, &remote, &workspace)
        .await
        .expect("push to R2 via presign must succeed");
    eprintln!(
        "smoke_r2_presign: pushed {} blobs (0 = all content already present on R2)",
        upload_count
    );

    // --- PULL via presign path ---

    let store_b = MemStore::new();
    let root_b = pull(&remote, &workspace, &store_b)
        .await
        .expect("pull from R2 via presign must succeed");

    // --- CLEANUP ---

    let r2: Arc<dyn ObjectStore> = Arc::new(
        AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_access_key_id(&access_key)
            .with_secret_access_key(&secret_key)
            .with_region(&region)
            .with_endpoint(&endpoint)
            .with_allow_http(true)
            .build()
            .expect("build R2 object_store handle for cleanup"),
    );

    let ref_key = ObjPath::from(format!("ref/{}", workspace));
    match r2.delete(&ref_key).await {
        Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
        Err(e) => eprintln!("smoke_r2_presign: cleanup ref warning: {}", e),
    }

    let mut deleted = 0usize;
    for hash in &fixture.all_hashes {
        let hex = hash_to_hex(hash);
        let blob_key = ObjPath::from(format!("blobs/{}/{}", &hex[..2], &hex[2..]));
        match r2.delete(&blob_key).await {
            Ok(_) => {
                deleted += 1;
            }
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => eprintln!("smoke_r2_presign: cleanup blob warning for {}: {}", hex, e),
        }
    }
    eprintln!("smoke_r2_presign: cleaned up {} blobs + ref object", deleted);

    // --- FINAL ASSERTIONS ---

    assert_eq!(
        fixture.root, root_b,
        "pulled root {} must equal pushed root {}",
        hash_to_hex(&root_b),
        hash_to_hex(&fixture.root)
    );

    let index_a = Index::build(&fixture.store, &fixture.root).expect("index A must build");
    let index_b = Index::build(&store_b, &root_b).expect("index B must build");

    assert_eq!(index_a.len(), index_b.len(), "file count must match after R2 presign roundtrip");

    for (path, hash_a) in index_a.entries() {
        let hash_b = index_b
            .lookup(path)
            .unwrap_or_else(|| panic!("path {} missing from pulled store B", path));
        assert_eq!(*hash_a, hash_b, "hash mismatch for {} after R2 presign roundtrip", path);
        let content_a = fixture.store.get(hash_a).unwrap().unwrap();
        let content_b = store_b.get(&hash_b).unwrap().unwrap();
        assert_eq!(content_a, content_b, "byte content mismatch for {}", path);
    }

    eprintln!("smoke_r2_presign: all assertions passed");
}
