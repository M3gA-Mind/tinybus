use std::io::Write;
use std::time::Duration;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn written(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("lib.so");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn hex(path: &std::path::Path, age: Duration) -> (String, bool) {
    super::imp::hex(std::fs::File::open(path).unwrap(), age).unwrap()
}

#[test]
fn an_untouched_file_is_hashed_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = written(dir.path(), &pattern(300_000));
    let first = hex(&path, Duration::ZERO);
    let second = hex(&path, Duration::ZERO);
    assert!(!first.1, "the first digest is computed");
    assert!(second.1, "the second is remembered");
    assert_eq!(first.0, second.0);
}

#[test]
fn a_file_rewritten_in_place_is_hashed_again_even_at_the_same_length() {
    let dir = tempfile::tempdir().unwrap();
    let original = pattern(300_000);
    let path = written(dir.path(), &original);
    let before = hex(&path, Duration::ZERO).0;

    std::thread::sleep(Duration::from_millis(20));
    let mut tampered = original.clone();
    tampered[1234] ^= 0xff;
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(&tampered).unwrap();
    drop(file);

    let (after, remembered) = hex(&path, Duration::ZERO);
    assert!(!remembered, "a changed file must not be served from memory");
    assert_ne!(after, before);
    assert_eq!(after, crate::module::hash::file_hex(&tampered[..]).unwrap());
}

#[test]
fn a_file_touched_inside_the_quiet_period_is_never_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let path = written(dir.path(), &pattern(1000));
    let age = Duration::from_secs(3600);
    assert!(!hex(&path, age).1);
    assert!(!hex(&path, age).1, "too fresh to trust a remembered digest");
}

#[test]
fn the_public_entry_point_returns_the_same_digest() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(70_000);
    let path = written(dir.path(), &data);
    let via_memory = super::file_hex_remembered(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(
        via_memory,
        crate::module::hash::file_hex(&data[..]).unwrap()
    );
}
