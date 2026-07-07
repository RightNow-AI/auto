//! Container format properties: any canonical entry map round-trips through
//! bytes unchanged with a stable id, and every malformed byte stream is
//! refused with the right error — the id is the trust anchor, so the format
//! must be exact.

use std::collections::BTreeMap;

use auto_backend::{Artifact, ContainerError, MAGIC, MANIFEST_ENTRY, MODULE_ENTRY};
use proptest::collection::{btree_map, vec as bytes_vec};
use proptest::prelude::*;

/// Arbitrary entry maps that always carry the required entries.
fn entries_strategy() -> impl Strategy<Value = BTreeMap<String, Vec<u8>>> {
    (
        btree_map("[a-z0-9._-]{1,16}", bytes_vec(any::<u8>(), 0..64), 0..6),
        bytes_vec(any::<u8>(), 0..64),
        bytes_vec(any::<u8>(), 0..64),
    )
        .prop_map(|(mut entries, manifest, module)| {
            entries.insert(MANIFEST_ENTRY.to_owned(), manifest);
            entries.insert(MODULE_ENTRY.to_owned(), module);
            entries
        })
}

proptest! {
    #[test]
    fn roundtrip_and_stable_id(entries in entries_strategy()) {
        let artifact = Artifact::new(entries);
        let bytes = artifact.to_bytes();
        let parsed = Artifact::from_bytes(&bytes).expect("canonical bytes parse");
        prop_assert_eq!(&parsed, &artifact);
        // two serializations of equal artifacts: identical bytes, one id
        prop_assert_eq!(&parsed.to_bytes(), &bytes);
        prop_assert_eq!(parsed.id(), artifact.id());
    }
}

fn minimal_bytes() -> Vec<u8> {
    Artifact::new(BTreeMap::from([
        (MANIFEST_ENTRY.to_owned(), b"MAN".to_vec()),
        (MODULE_ENTRY.to_owned(), b"MOD".to_vec()),
    ]))
    .to_bytes()
}

#[test]
fn bad_magic_is_refused() {
    let mut bytes = minimal_bytes();
    bytes[0] = b'X';
    assert!(matches!(
        Artifact::from_bytes(&bytes),
        Err(ContainerError::BadMagic)
    ));
}

#[test]
fn every_proper_prefix_is_refused() {
    let bytes = minimal_bytes();
    for cut in 0..bytes.len() {
        let err = Artifact::from_bytes(&bytes[..cut]).expect_err("prefix must not parse");
        if cut < MAGIC.len() {
            assert!(matches!(err, ContainerError::BadMagic), "cut {cut}: {err}");
        } else {
            assert!(
                matches!(err, ContainerError::Truncated(_)),
                "cut {cut}: {err}"
            );
        }
    }
}

#[test]
fn truncation_names_the_missing_field() {
    let bytes = minimal_bytes();
    // layout: magic 0..4 | count 4..8 | name_len 8..12 | name 12..25 |
    // data_len 25..33 | data 33..36 | second entry 36..
    for (cut, what) in [
        (6, "entry count"),
        (10, "name length"),
        (20, "name"),
        (28, "data length"),
        (34, "entry data"),
        (36, "name length"), // count says 2; the second entry never starts
    ] {
        match Artifact::from_bytes(&bytes[..cut]) {
            Err(ContainerError::Truncated(found)) => {
                assert_eq!(found, what, "cut {cut}");
            }
            other => panic!("cut {cut}: expected Truncated({what:?}), got {other:?}"),
        }
    }
}

#[test]
fn trailing_bytes_are_refused() {
    let mut bytes = minimal_bytes();
    bytes.push(0);
    assert!(matches!(
        Artifact::from_bytes(&bytes),
        Err(ContainerError::TrailingBytes)
    ));
}

#[test]
fn missing_required_entries_are_refused() {
    let only_manifest = Artifact::new(BTreeMap::from([(
        MANIFEST_ENTRY.to_owned(),
        b"MAN".to_vec(),
    )]))
    .to_bytes();
    assert!(matches!(
        Artifact::from_bytes(&only_manifest),
        Err(ContainerError::MissingEntry(MODULE_ENTRY))
    ));

    let only_module =
        Artifact::new(BTreeMap::from([(MODULE_ENTRY.to_owned(), b"MOD".to_vec())])).to_bytes();
    assert!(matches!(
        Artifact::from_bytes(&only_module),
        Err(ContainerError::MissingEntry(MANIFEST_ENTRY))
    ));
}

fn push_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    out.extend_from_slice(&u32::try_from(name.len()).expect("short name").to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}

#[test]
fn unsorted_entries_are_not_canonical() {
    // module.wasm before manifest.json: valid framing, wrong order
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    push_entry(&mut bytes, MODULE_ENTRY, b"MOD");
    push_entry(&mut bytes, MANIFEST_ENTRY, b"MAN");
    match Artifact::from_bytes(&bytes) {
        Err(ContainerError::NotCanonical(name)) => assert_eq!(name, MANIFEST_ENTRY),
        other => panic!("expected NotCanonical, got {other:?}"),
    }
}

#[test]
fn duplicate_entries_are_not_canonical() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    push_entry(&mut bytes, MANIFEST_ENTRY, b"MAN");
    push_entry(&mut bytes, MANIFEST_ENTRY, b"MAN");
    match Artifact::from_bytes(&bytes) {
        Err(ContainerError::NotCanonical(name)) => assert_eq!(name, MANIFEST_ENTRY),
        other => panic!("expected NotCanonical, got {other:?}"),
    }
}
