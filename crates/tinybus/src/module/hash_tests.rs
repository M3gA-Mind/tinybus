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
