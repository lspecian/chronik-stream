//! GCS and Azure backends (OpenDAL) against their emulators.
//!
//! Ignored by default: they need a running emulator.
//!
//! ```sh
//! docker run -d --rm --name fake-gcs -p 4443:4443 fsouza/fake-gcs-server -scheme http
//! docker run -d --rm --name azurite -p 10010:10000 \
//!     mcr.microsoft.com/azure-storage/azurite azurite-blob --blobHost 0.0.0.0 --skipApiVersionCheck
//! GCS_EMULATOR=http://127.0.0.1:4443 AZURITE=http://127.0.0.1:10010 \
//!     cargo test -p chronik-storage --test opendal_backends_test -- --ignored
//! ```
//!
//! Each run uses a fresh bucket/container (GCS) or prefix (Azure), so reruns
//! against the same emulator do not see earlier objects.

use chronik_common::Bytes;
use chronik_storage::object_store::{
    AuthConfig, AzureCredentials, GcsCredentials, ObjectStore, ObjectStoreConfig,
    ObjectStoreError, ObjectStoreFactory, StorageBackend,
};

fn unique(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4().simple())
}

/// The round trip every backend must pass: nested keys, reads, ranges,
/// metadata, recursive listing by prefix, copy, delete, NotFound.
async fn exercise(store: &dyn ObjectStore) {
    let a = Bytes::from_static(b"0123456789");
    let b = Bytes::from_static(b"segment-b");
    let c = Bytes::from_static(b"segment-c");
    store.put("topics/t1/p0/seg-a", a.clone()).await.unwrap();
    store.put("topics/t1/p0/deep/seg-b", b.clone()).await.unwrap();
    store.put("topics/t1/p1/seg-c", c.clone()).await.unwrap();
    store.put("other/seg-d", Bytes::from_static(b"d")).await.unwrap();

    assert_eq!(store.get("topics/t1/p0/seg-a").await.unwrap(), a);
    assert_eq!(store.get_range("topics/t1/p0/seg-a", 2, 3).await.unwrap(), Bytes::from_static(b"234"));

    let head = store.head("topics/t1/p0/deep/seg-b").await.unwrap();
    assert_eq!(head.size, b.len() as u64);
    assert!(head.last_modified > 0, "last_modified missing");

    assert!(store.exists("topics/t1/p1/seg-c").await.unwrap());
    assert!(!store.exists("topics/t1/p1/missing").await.unwrap());

    // Recursive, by prefix, files only: the nested key is included and no
    // directory placeholder is.
    let mut keys: Vec<String> = store.list("topics/t1/").await.unwrap().into_iter().map(|m| m.key).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["topics/t1/p0/deep/seg-b", "topics/t1/p0/seg-a", "topics/t1/p1/seg-c"],
        "recursive list by prefix"
    );
    let sized = store.list("topics/t1/p1/").await.unwrap();
    assert_eq!(sized.len(), 1);
    assert_eq!(sized[0].size, c.len() as u64, "list reports sizes");

    store.copy("topics/t1/p1/seg-c", "topics/t1/p1/seg-c.copy").await.unwrap();
    assert_eq!(store.get("topics/t1/p1/seg-c.copy").await.unwrap(), c);

    store.delete("topics/t1/p1/seg-c").await.unwrap();
    assert!(!store.exists("topics/t1/p1/seg-c").await.unwrap());
    match store.get("topics/t1/p1/seg-c").await {
        Err(ObjectStoreError::NotFound { .. }) => {}
        other => panic!("expected NotFound after delete, got {other:?}"),
    }
}

/// A store over the same bucket with a different prefix sees none of it.
async fn assert_prefix_isolates(a: &dyn ObjectStore, b: &dyn ObjectStore) {
    a.put("iso/key", Bytes::from_static(b"x")).await.unwrap();
    assert!(a.exists("iso/key").await.unwrap());
    assert!(!b.exists("iso/key").await.unwrap(), "prefix not applied: object visible under another prefix");
    assert!(b.list("iso/").await.unwrap().is_empty());
}

fn gcs_config(endpoint: &str, bucket: &str, prefix: &str) -> ObjectStoreConfig {
    ObjectStoreConfig {
        backend: StorageBackend::Gcs { project_id: None, endpoint: Some(endpoint.to_string()) },
        bucket: bucket.to_string(),
        prefix: Some(prefix.to_string()),
        auth: AuthConfig::Gcs(GcsCredentials::Anonymous),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs fake-gcs-server; set GCS_EMULATOR"]
async fn gcs_backend_round_trip() {
    let Ok(endpoint) = std::env::var("GCS_EMULATOR") else {
        eprintln!("SKIP: GCS_EMULATOR not set");
        return;
    };
    // fake-gcs-server needs the bucket to exist; create it over its JSON API.
    let bucket = unique("chronik");
    let resp = reqwest::Client::new()
        .post(format!("{endpoint}/storage/v1/b"))
        .json(&serde_json::json!({ "name": bucket }))
        .send()
        .await
        .expect("create bucket");
    assert!(resp.status().is_success(), "create bucket: {}", resp.status());

    let store = ObjectStoreFactory::create(gcs_config(&endpoint, &bucket, "run")).await.unwrap();
    exercise(store.as_ref()).await;
    let other = ObjectStoreFactory::create(gcs_config(&endpoint, &bucket, "other-run")).await.unwrap();
    assert_prefix_isolates(store.as_ref(), other.as_ref()).await;
}

fn azure_config(blob: &str, container: &str, prefix: &str) -> ObjectStoreConfig {
    ObjectStoreConfig {
        backend: StorageBackend::Azure {
            account_name: "devstoreaccount1".to_string(),
            endpoint: Some(format!("{blob}/devstoreaccount1")),
            use_emulator: true,
        },
        bucket: container.to_string(),
        prefix: Some(prefix.to_string()),
        // DefaultChain + use_emulator = the emulator's development key, as
        // the server configures it from AZURE_USE_EMULATOR.
        auth: AuthConfig::Azure(AzureCredentials::DefaultChain),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs Azurite; set AZURITE to its blob URL"]
async fn azure_backend_round_trip() {
    let Ok(blob) = std::env::var("AZURITE") else {
        eprintln!("SKIP: AZURITE not set");
        return;
    };
    // Blob storage does not create a container on first PUT.
    let container = unique("chronik");
    create_azurite_container(&blob, &container).await;
    let prefix = unique("run");
    let store = ObjectStoreFactory::create(azure_config(&blob, &container, &prefix)).await.unwrap();
    exercise(store.as_ref()).await;
    let other = ObjectStoreFactory::create(azure_config(&blob, &container, &unique("other"))).await.unwrap();
    assert_prefix_isolates(store.as_ref(), other.as_ref()).await;
}

/// Create a container in Azurite with a SharedKey-signed request (the
/// development account and key; path-style URL).
async fn create_azurite_container(blob: &str, container: &str) {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    const ACCOUNT: &str = "devstoreaccount1";
    const KEY: &str =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
    const VERSION: &str = "2021-08-06";
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    // VERB, 11 standard headers (all empty; zero Content-Length is empty),
    // canonical x-ms-* headers, canonical resource (account + URI path).
    let string_to_sign = format!(
        "PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:{VERSION}\n/{ACCOUNT}/{ACCOUNT}/{container}\nrestype:container"
    );
    let key = base64::engine::general_purpose::STANDARD.decode(KEY).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(string_to_sign.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let resp = reqwest::Client::new()
        .put(format!("{blob}/{ACCOUNT}/{container}?restype=container"))
        .header("x-ms-date", &date)
        .header("x-ms-version", VERSION)
        .header("Content-Length", "0")
        .header("Authorization", format!("SharedKey {ACCOUNT}:{signature}"))
        .send()
        .await
        .expect("create container");
    let status = resp.status();
    assert!(status.as_u16() == 201, "create container: {status} {}", resp.text().await.unwrap_or_default());
}
