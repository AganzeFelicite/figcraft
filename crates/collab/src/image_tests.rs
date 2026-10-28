//! Image entries are checked as they apply: bytes carried under a content-hash key must be
//! exactly what that key hashes to (lowercase sha256, the form `figcraft_format::image_hash`
//! produces), a side stays within the 65 535 px bound the implementation enforces and decoded
//! bytes within its 32 MiB cap. Without this a collaborator could poison the shared
//! content-addressed image store or balloon memory (#1086).

use base64::Engine as _;
use figcraft_doc::Document;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::change::{Change, Entry, Json};
use crate::docprops::apply as apply_doc;
use crate::docprops::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE};

/// The side bound the implementation enforces.
const MAX_SIDE: u32 = MAX_IMAGE_SIDE;
/// The decoded-bytes cap the implementation enforces.
const MAX_BYTES: usize = MAX_IMAGE_BYTES;

/// sha256 hex, computed here independently of the implementation under test.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn put(key: &str, width: u32, height: u32, bytes: &[u8]) -> Change {
    Change::Put {
        entry: Entry::Image(key.into()),
        value: Json(json!({
            "width": width,
            "height": height,
            "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
        })),
    }
}

/// Bytes a collaborator hashed into the key, and different bytes they send under it.
fn forged() -> (String, &'static [u8]) {
    (sha256_hex(b"the image the key actually names"), b"someone else's bytes")
}

#[test]
fn bytes_that_do_not_hash_to_their_content_hash_key_are_refused() {
    let (key, bytes) = forged();
    let mut doc = Document::new("t");
    let e = match apply_doc(&mut doc, &put(&key, 2, 1, bytes)) {
        Ok(_) => panic!("bytes that don't hash to their key must be refused"),
        Err(e) => e,
    };
    assert!(e.to_string().contains("image"), "not an image refusal: {e}");
    assert!(doc.images.is_empty(), "the image store was poisoned under key {key}");
}

/// An uppercase key looks like a content hash but isn't one: producers store the lowercase
/// form, so bytes matching under the uppercase form would file a second copy under a name
/// no honest lookup reaches. Refused (#1086).
#[test]
fn an_uppercase_content_hash_shaped_key_is_refused() {
    let bytes = b"honestly keyed, uppercase claim".as_slice();
    let mut doc = Document::new("t");
    let upper = sha256_hex(bytes).to_uppercase();
    let e = match apply_doc(&mut doc, &put(&upper, 2, 1, bytes)) {
        Ok(_) => panic!("an uppercase key claims a content address it doesn't have"),
        Err(e) => e,
    };
    assert!(e.to_string().contains("image"), "not an image refusal: {e}");
    assert!(doc.images.is_empty(), "the store took an uppercase-keyed copy");
}

#[test]
fn an_image_that_hashes_to_its_key_applies() {
    let bytes = b"a real image, honestly keyed".as_slice();
    let mut doc = Document::new("t");
    apply_doc(&mut doc, &put(&sha256_hex(bytes), 2, 1, bytes)).expect("an honest image must apply unchanged");
    let img = doc.images.get(&sha256_hex(bytes)).expect("the image is stored");
    assert_eq!((img.width, img.height), (2, 1));
    assert_eq!(img.bytes.as_slice(), bytes);
}

#[test]
fn absurd_dimensions_are_refused() {
    let bytes = b"honest bytes, dishonest size".as_slice();
    let key = sha256_hex(bytes);
    for (w, h) in [(MAX_SIDE + 1, 10), (10, MAX_SIDE + 1), (u32::MAX, u32::MAX)] {
        let mut doc = Document::new("t");
        apply_doc(&mut doc, &put(&key, w, h, bytes))
            .err()
            .unwrap_or_else(|| panic!("a {w}×{h} image must be refused, but it applied to {:?} keys", doc.images.len()));
        assert!(doc.images.is_empty());
    }
}

#[test]
fn oversized_image_bytes_are_refused() {
    let bytes = vec![0u8; MAX_BYTES + 1];
    let key = sha256_hex(&bytes);
    let mut doc = Document::new("t");
    apply_doc(&mut doc, &put(&key, 100, 100, &bytes)).err().unwrap_or_else(|| panic!("a {}-byte image must be refused, but it applied", bytes.len()));
    assert!(doc.images.is_empty());
}

#[test]
fn a_zero_sized_image_still_applies() {
    // The format crate stores 0×0 for bytes it can't probe (`image_data` in `figcraft_format`),
    // so a shared document can legitimately carry one; the bytes still have to be honest.
    let bytes = b"undecodable bytes kept from a file load".as_slice();
    let mut doc = Document::new("t");
    apply_doc(&mut doc, &put(&sha256_hex(bytes), 0, 0, bytes)).expect("0×0 means the size is unknown, not absurd");
    assert!(doc.images.contains_key(&sha256_hex(bytes)));
}

#[test]
fn a_key_that_is_not_a_content_hash_shape_still_applies() {
    // Legacy producers key images without sha256: the svg importer stores FNV-128 keys (32 hex
    // chars, re-keyed by sha256 on save), and the REST importer keeps a placeholder under the
    // imageRef itself. They are not content-addressed references, so no hash is claimed; the
    // dimension and size bounds still hold.
    for key in ["rest-image-missing", "hash", &"f".repeat(32), &"ab".repeat(20)] {
        let mut doc = Document::new("t");
        apply_doc(&mut doc, &put(key, 2, 1, b"placeholder bytes")).unwrap_or_else(|e| panic!("legacy key {key:?} refused: {e}"));
        assert!(doc.images.contains_key(key));
        let mut big = Document::new("t");
        assert!(apply_doc(&mut big, &put(key, MAX_SIDE + 1, 1, b"placeholder bytes")).is_err(), "a legacy key must still bound dimensions");
    }
}

/// A key becomes an archive member name when the file is saved (`images/<key>`), so a key that
/// isn't plain letters and digits is refused as it applies: a collaborator could otherwise write
/// `images/../../x` into everyone's saved file, which the next load drops as damaged (#1086).
#[test]
fn a_key_that_cannot_name_an_archive_member_is_refused() {
    for key in ["../../evil", "a/b", "x.png", "", "with space", "\\server", &"a".repeat(129)] {
        let mut doc = Document::new("t");
        assert!(apply_doc(&mut doc, &put(key, 2, 1, b"placeholder bytes")).is_err(), "key {key:?} must be refused");
        assert!(doc.images.is_empty(), "key {key:?} reached the image store");
    }
}
