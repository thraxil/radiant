//! Behaviour every [`BlobStore`] must have. Each backend's tests call [`run`].

use bytes::Bytes;

use super::{BlobName, BlobStore};
use crate::format::Format;
use crate::hash::Sha1Hash;

pub(crate) async fn run(store: &dyn BlobStore) {
    let original = Bytes::from_static(b"original bytes");
    let hash = Sha1Hash::digest(&original);

    // Missing things are None, not errors.
    assert!(store.find_original(&hash).await.unwrap().is_none());

    let name = BlobName::Original {
        hash,
        format: Format::Png,
    };
    let r = store.put(&name, original.clone()).await.unwrap();
    assert_eq!(store.get(&r).await.unwrap(), Some(original.clone()));

    // put is idempotent and returns the same ref.
    assert_eq!(store.put(&name, original.clone()).await.unwrap(), r);

    // Variants don't collide with the original or with each other.
    let v1 = store
        .put(
            &BlobName::Variant {
                hash,
                spec: "100s".parse().unwrap(),
                format: Format::Png,
            },
            Bytes::from_static(b"v1"),
        )
        .await
        .unwrap();
    let v2 = store
        .put(
            &BlobName::Variant {
                hash,
                spec: "100s".parse().unwrap(),
                format: Format::Webp,
            },
            Bytes::from_static(b"v2"),
        )
        .await
        .unwrap();
    assert_ne!(v1, r);
    assert_ne!(v1, v2);
    assert_eq!(store.get(&v1).await.unwrap().unwrap(), "v1");
    assert_eq!(store.get(&v2).await.unwrap().unwrap(), "v2");
    assert_eq!(store.get(&r).await.unwrap(), Some(original.clone()));

    // A full-size conversion to another format sits next to the original and must not be
    // mistaken for it. Write it after the original so it is not simply listed second.
    let full_jpg = store
        .put(
            &BlobName::Variant {
                hash,
                spec: "full".parse().unwrap(),
                format: Format::Jpeg,
            },
            Bytes::from_static(b"converted"),
        )
        .await
        .unwrap();
    assert_ne!(full_jpg, r);

    // An original can be found from its hash alone, even with variants next to it.
    let (found_ref, found) = store.find_original(&hash).await.unwrap().unwrap();
    assert_eq!(found_ref, r);
    assert_eq!(found, original);

    // Another hash is still not found.
    assert!(
        store
            .find_original(&Sha1Hash::digest(b"other"))
            .await
            .unwrap()
            .is_none()
    );
}
