use super::*;
use crate::hbk_writer::{build_container, build_zip};

const FILES: &[(&str, &str)] = &[
    ("objects/catalog1/Thing.html", "<html>thing page, long enough to span blocks</html>"),
    ("objects/catalog1/Thing/methods/Do.html", "<html>do page</html>"),
    ("objects/Global context/methods/Fn.html", "<html>global page</html>"),
    ("struct_If.st", "st"),
];

fn write_hbk(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("test.hbk");
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn chained_compressed_container_unpacks_to_the_written_files() {
    let zip = build_zip(FILES);
    let container = build_container(
        &[("Book", Some(b"{book}")), (FILE_STORAGE, Some(&zip)), ("PackBlock", None)],
        64,
    );
    let dir = tempfile::tempdir().unwrap();
    let hbk = write_hbk(dir.path(), &container);

    let opened = HbkContainer::open(&hbk).unwrap();
    assert_eq!(opened.entity_names().collect::<Vec<_>>(), ["Book", FILE_STORAGE, "PackBlock"]);
    assert_eq!(opened.entity("Book").unwrap(), b"{book}");
    assert!(opened.entity("PackBlock").is_err(), "an entity without a body has no content");

    let dest = dir.path().join("out");
    assert_eq!(extract_file_storage(&hbk, &dest).unwrap(), FILES.len());
    for (name, content) in FILES {
        assert_eq!(fs::read_to_string(dest.join(name)).unwrap(), *content, "{name}");
    }
}

#[test]
fn corrupt_containers_are_errors_not_hangs() {
    let zip = build_zip(FILES);
    let good = build_container(&[(FILE_STORAGE, Some(&zip))], 64);
    let dir = tempfile::tempdir().unwrap();
    let extract = |bytes: &[u8], tag: &str| {
        let hbk = dir.path().join(format!("{tag}.hbk"));
        fs::write(&hbk, bytes).unwrap();
        extract_file_storage(&hbk, &dir.path().join(tag))
    };

    assert!(extract(&good[..10], "short").is_err());

    // The descriptor points its entity header past the end of the file.
    let mut out_of_range = good.clone();
    let descriptor = CONTAINER_HEADER_SIZE + BLOCK_HEADER_SIZE;
    out_of_range[descriptor..descriptor + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(extract(&out_of_range, "range").is_err());

    // The second block of the storage chain points back to itself.
    let body = u32_le(&good, descriptor + 4) as usize;
    let second =
        usize::from_str_radix(std::str::from_utf8(&good[body + 20..body + 28]).unwrap(), 16)
            .unwrap();
    let mut cycle = good.clone();
    cycle[second + 20..second + 28].copy_from_slice(format!("{second:08x}").as_bytes());
    assert!(extract(&cycle, "cycle").unwrap_err().to_string().contains("loops"));

    // The chain ends before the declared payload.
    let mut truncated = good.clone();
    truncated[body + 20..body + 28].copy_from_slice(format!("{SPLITTER:08x}").as_bytes());
    assert!(extract(&truncated, "truncated").unwrap_err().to_string().contains("ended after"));

    // A damaged deflate stream inside the storage archive.
    let mut damaged_zip = zip.clone();
    let middle = damaged_zip.len() / 3;
    for byte in &mut damaged_zip[middle..middle + 16] {
        *byte ^= 0xA5;
    }
    let damaged = build_container(&[(FILE_STORAGE, Some(&damaged_zip))], 64);
    assert!(extract(&damaged, "damaged").is_err());

    let no_storage = build_container(&[("Book", Some(b"{}"))], 64);
    assert!(extract(&no_storage, "nostorage").unwrap_err().to_string().contains("absent"));
}

#[test]
fn entries_escaping_the_destination_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (tag, name) in [("parent", "../evil.html"), ("absolute", "/tmp/evil.html")] {
        let zip = build_zip(&[("objects/ok.html", "ok"), (name, "evil")]);
        let hbk = dir.path().join(format!("{tag}.hbk"));
        fs::write(&hbk, build_container(&[(FILE_STORAGE, Some(&zip))], 4096)).unwrap();
        let dest = dir.path().join(tag).join("out");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let error = extract_file_storage(&hbk, &dest).unwrap_err();
        assert!(error.to_string().contains("escapes"), "{error}");
        assert!(!dir.path().join(tag).join("evil.html").exists());
        assert!(!dest.exists(), "the entry written before the refusal is removed too");
    }
}
