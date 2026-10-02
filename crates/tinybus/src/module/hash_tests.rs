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

/// Fails with `kind` on the first `read`, then behaves like `data`.
struct FailsOnce<'a> {
    data: &'a [u8],
    kind: Option<std::io::ErrorKind>,
}

impl std::io::Read for FailsOnce<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Some(kind) = self.kind.take() {
            return Err(kind.into());
        }
        let n = out.len().min(self.data.len());
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn an_interrupted_read_is_retried_not_reported() {
    let data = pattern(5000);
    let reader = FailsOnce {
        data: &data,
        kind: Some(std::io::ErrorKind::Interrupted),
    };
    assert_eq!(
        super::file_hex(reader).unwrap(),
        super::file_hex(&data[..]).unwrap()
    );
}

#[test]
fn a_failing_read_is_an_error_not_a_digest_of_what_was_read() {
    let data = pattern(5000);
    let reader = FailsOnce {
        data: &data,
        kind: Some(std::io::ErrorKind::PermissionDenied),
    };
    let error = super::file_hex(reader).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}
