/// Smoke test for real S3/R2 cloud connectivity.
/// Skipped unless LUNAR_SMOKE=1 is set in the environment.
/// Run via: scripts/smoke-remote.sh or LUNAR_SMOKE=1 cargo test --test smoke_remote
use devdropbox::cas::{hash_bytes, hash_to_hex};
use devdropbox::remote::Remote;
use std::sync::Arc;

#[test]
fn smoke_s3_remote() {
    if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
        return; // skipped: set LUNAR_SMOKE=1 to run this test
    }

    let bucket = std::env::var("LUNAR_REMOTE_BUCKET")
        .expect("LUNAR_REMOTE_BUCKET must be set when LUNAR_SMOKE=1");
    let access_key =
        std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID must be set");
    let secret_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY must be set");
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let endpoint = std::env::var("LUNAR_REMOTE_ENDPOINT").ok();

    println!(
        "smoke: bucket={} region={} endpoint={:?}",
        bucket, region, endpoint
    );

    let mut builder = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name(&bucket)
        .with_access_key_id(&access_key)
        .with_secret_access_key(&secret_key)
        .with_region(&region);

    if let Some(ep) = &endpoint {
        builder = builder.with_endpoint(ep);
    }

    let store = builder.build().expect("S3/R2 builder must succeed");
    let remote = Remote::from_store(Arc::new(store), "smoke-ws")
        .expect("Remote::from_store must succeed");

    // Blob round-trip
    let data = b"lunar smoke test payload";
    let hash = remote.put_blob(data).expect("put_blob must succeed");
    let got = remote.get_blob(&hash).expect("get_blob must succeed");
    assert_eq!(got, Some(data.to_vec()), "blob must round-trip");
    println!("smoke: blob ok hash={}", hash_to_hex(&hash));

    // HEAD pointer round-trip (handle pre-existing HEAD from a prior run)
    let root = hash_bytes(b"smoke root hash");
    let maybe_head = remote.read_head().expect("read_head must not error");
    let ptr = if let Some(existing) = maybe_head {
        println!("smoke: HEAD already exists, updating");
        remote
            .update_head(&root, &existing.version)
            .expect("update_head must succeed")
    } else {
        println!("smoke: initializing HEAD");
        remote.init_head(&root).expect("init_head must succeed")
    };

    let got_head = remote.read_head().unwrap().expect("HEAD must exist after write");
    assert_eq!(got_head.root, root, "read_head must return the written root");
    println!("smoke: HEAD ok root={}", hash_to_hex(&ptr.root));

    println!("smoke: all assertions passed");
}
