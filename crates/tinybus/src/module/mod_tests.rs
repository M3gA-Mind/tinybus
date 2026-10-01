#[test]
fn public_file_hashing_uses_the_same_sha256_implementation_as_module_admission() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("asset.tar.gz");
    std::fs::write(&path, b"asset").unwrap();
    assert_eq!(
        super::sha256_file(path).unwrap(),
        "d59386e0ae435e292fbe0ebcdb954b75ed5fb3922091277cb19f798fc5d50718"
    );
}
