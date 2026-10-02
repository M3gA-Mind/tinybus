#[test]
fn sha256_matches_the_published_empty_input_vector() {
    assert_eq!(
        super::file_hex(&b""[..]).unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_matches_the_published_56_byte_input_vector() {
    assert_eq!(
        super::file_hex(&[b'a'; 56][..]).unwrap(),
        "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
    );
}

#[test]
fn sha256_matches_a_published_multi_block_input_vector() {
    assert_eq!(
        super::file_hex(&[b'a'; 1000][..]).unwrap(),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

/// Hands out at most `step` bytes per `read`, the way a pipe or a slow disk can.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl std::io::Read for Trickle<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.step.min(out.len()).min(self.data.len());
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn sha256_is_the_same_whatever_the_read_sizes_and_chunk_boundaries() {
    // Spans several chunks and ends mid-chunk and mid-block.
    let data = pattern(super::CHUNK * 3 + 77);
    let whole = super::file_hex(&data[..]).unwrap();
    for step in [1, 63, 64, 65, 4096, super::CHUNK - 1, super::CHUNK + 1] {
        let trickled = super::file_hex(Trickle { data: &data, step }).unwrap();
        assert_eq!(trickled, whole, "step {step}");
    }
}

#[test]
fn sha256_matches_the_published_million_a_vector() {
    assert_eq!(
        super::file_hex(&vec![b'a'; 1_000_000][..]).unwrap(),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[cfg(all(feature = "modules", unix))]
mod remembered {
    use std::io::Write;
    use std::time::Duration;

    use super::pattern;

    fn written(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("lib.so");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn hex(path: &std::path::Path, age: Duration) -> (String, bool) {
        crate::module::hash::remembered::hex(std::fs::File::open(path).unwrap(), age).unwrap()
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
        assert_eq!(after, super::super::file_hex(&tampered[..]).unwrap());
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
        let via_memory =
            crate::module::hash::file_hex_remembered(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(via_memory, super::super::file_hex(&data[..]).unwrap());
    }
}
