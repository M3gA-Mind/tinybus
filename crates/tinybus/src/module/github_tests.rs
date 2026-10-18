use super::*;
use std::io::Write;

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

#[test]
/// Extracts a valid archive while preserving its directory structure and bytes.
fn zip_archives_extract_only_enclosed_regular_files_and_directories() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("release.zip");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .add_directory("nested/", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive
        .start_file("nested/module.so", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"module bytes").unwrap();
    archive.finish().unwrap();

    extract("release.zip", &archive_path, &destination).unwrap();

    assert!(destination.join("nested").is_dir());
    assert_eq!(
        std::fs::read(destination.join("nested/module.so")).unwrap(),
        b"module bytes"
    );
}

#[test]
/// Rejects traversal entries without writing outside the extraction directory.
fn zip_archives_refuse_paths_outside_the_destination() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("release.zip");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("../outside", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"outside").unwrap();
    archive.finish().unwrap();

    assert!(extract("release.zip", &archive_path, &destination).is_err());
    assert!(!directory.path().join("outside").exists());
}

#[cfg(unix)]
#[test]
/// Rejects symlink entries without creating a link or following its target.
fn zip_archives_refuse_symlink_entries() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("release.zip");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .add_symlink(
            "module.so",
            "../../outside",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    archive.finish().unwrap();

    assert!(extract("release.zip", &archive_path, &destination).is_err());
    assert!(!directory.path().join("outside").exists());
    assert!(!destination.join("module.so").exists());
}

#[cfg(unix)]
#[test]
/// Rejects Unix special-file entries instead of materializing them.
fn zip_archives_refuse_special_file_entries() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("release.zip");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file(
            "module.so",
            zip::write::SimpleFileOptions::default().external_attributes((0o010644_u32) << 16),
        )
        .unwrap();
    archive.write_all(b"not a regular file").unwrap();
    archive.finish().unwrap();

    assert!(extract("release.zip", &archive_path, &destination).is_err());
    assert!(!destination.join("module.so").exists());
}

#[cfg(unix)]
#[test]
/// Extracts regular files without applying archived executable or setuid bits.
fn zip_archives_do_not_apply_setuid_or_executable_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("release.zip");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file(
            "module.so",
            zip::write::SimpleFileOptions::default().unix_permissions(0o4755),
        )
        .unwrap();
    archive.write_all(b"module bytes").unwrap();
    archive.finish().unwrap();

    extract("release.zip", &archive_path, &destination).unwrap();

    let mode = std::fs::metadata(destination.join("module.so"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7000, 0, "special permission bits are not applied");
    assert_eq!(mode & 0o111, 0, "executable permission is not applied");
}
