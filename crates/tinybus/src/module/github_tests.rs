use super::*;

#[test]
fn github_tag_urls_parse_without_accepting_other_hosts_or_paths() {
    assert_eq!(
        parse_release_url("https://github.com/tinyhumansai/rust-template/releases/tag/v0.1.2")
            .unwrap(),
        ("tinyhumansai", "rust-template", "v0.1.2")
    );
    assert!(parse_release_url("https://example.com/a/b/releases/tag/v1").is_err());
    assert!(parse_release_url("https://github.com/a/b/releases/latest").is_err());
}

#[test]
fn checksum_manifests_accept_the_documented_toml_and_json_shapes() {
    let expected = "a".repeat(64);
    let toml = format!("[sha256]\n\"module.tar.gz\" = \"{expected}\"\n");
    assert_eq!(
        parse_checksums("checksum.toml", toml.as_bytes()).unwrap()["module.tar.gz"],
        expected
    );
    let json = format!(
        r###"{{"sha256":{{"module.tar.gz":"{}"}}}}"###,
        "b".repeat(64)
    );
    assert_eq!(
        parse_checksums("checksum.json", json.as_bytes()).unwrap()["module.tar.gz"],
        "b".repeat(64)
    );
}

#[test]
fn module_paths_are_canonicalized_before_admission() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join(if cfg!(windows) {
        "module.dll"
    } else if cfg!(target_os = "macos") {
        "module.dylib"
    } else {
        "module.so"
    });
    std::fs::write(&module, b"module").unwrap();

    let canonical = canonical_module(module.clone()).unwrap();
    assert_eq!(canonical, std::fs::canonicalize(module).unwrap());
}

#[test]
fn the_manifest_digest_must_agree_with_the_host_pin() {
    let digest = "c".repeat(64);
    let located = Located {
        manifest_name: "checksum.toml".to_string(),
        manifest: format!(
            "[sha256]\n\"m.tar.gz\" = \"{}\"\n",
            digest.to_ascii_uppercase()
        )
        .into_bytes(),
        archive_url: String::new(),
    };
    assert_eq!(manifest_digest(&located, "m.tar.gz", None).unwrap(), digest);
    assert_eq!(
        manifest_digest(&located, "m.tar.gz", Some(&digest)).unwrap(),
        digest
    );
    assert!(manifest_digest(&located, "m.tar.gz", Some(&"d".repeat(64))).is_err());
    assert!(manifest_digest(&located, "other.tar.gz", None).is_err());
    assert!(manifest_digest(&located, "m.tar.gz", Some("short")).is_err());
}

#[test]
fn a_cache_miss_with_downloads_disabled_is_refused_without_the_network() {
    let directory = tempfile::tempdir().unwrap();
    let error = acquire_cached(&CachedRelease {
        release_url: "https://github.com/tinyhumansai/demo/releases/tag/v1.0.0",
        asset_name: "demo-1.0.0.tar.gz",
        expected_sha256: None,
        cache_dir: &directory.path().join("demo").join("1.0.0"),
        allow_download: false,
    })
    .expect_err("nothing cached and no download allowed");
    assert!(
        error.to_string().contains("downloads are disabled"),
        "{error}"
    );
}

#[test]
fn a_verified_cache_answers_without_the_network() {
    let directory = tempfile::tempdir().unwrap();
    let cache_dir = directory.path().join("demo").join("1.0.0");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join("demo-1.0.0.tar.gz"), b"archive").unwrap();
    let module = cache_dir.join(format!("libdemo_module.{}", cache::library_extension()));
    std::fs::write(&module, b"library").unwrap();
    let digest = crate::module::sha256_file(cache_dir.join("demo-1.0.0.tar.gz")).unwrap();

    let (found, sha256) = acquire_cached(&CachedRelease {
        release_url: "https://github.com/tinyhumansai/demo/releases/tag/v1.0.0",
        asset_name: "demo-1.0.0.tar.gz",
        expected_sha256: Some(&digest),
        cache_dir: &cache_dir,
        // Downloads are off, so a hit is the only way this can succeed.
        allow_download: false,
    })
    .unwrap();
    assert_eq!(found, std::fs::canonicalize(module).unwrap());
    assert_eq!(sha256, digest);
}
