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
fn an_asset_name_that_leaves_the_cache_directory_is_refused() {
    // A verified archive one level above the version directory: a `..` asset
    // name used to reach it and answer from outside the cache.
    let directory = tempfile::tempdir().unwrap();
    let cache_dir = directory.path().join("demo").join("1.0.0");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let outside = directory.path().join("demo").join("escape.tar.gz");
    std::fs::write(&outside, b"archive").unwrap();
    std::fs::write(
        cache_dir.join(format!("libdemo_module.{}", cache::library_extension())),
        b"library",
    )
    .unwrap();
    let digest = crate::module::sha256_file(&outside).unwrap();

    let error = acquire_cached(&CachedRelease {
        release_url: "https://github.com/tinyhumansai/demo/releases/tag/v1.0.0",
        asset_name: "../escape.tar.gz",
        expected_sha256: Some(&digest),
        cache_dir: &cache_dir,
        allow_download: false,
    })
    .expect_err("an asset name outside the cache directory must be refused");
    assert!(
        error.to_string().contains("not a plain file name"),
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

#[cfg(unix)]
#[test]
fn release_extraction_tempdirs_are_private_regardless_of_umask() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    restrict_extraction_directory(directory.path()).unwrap();

    let mode = std::fs::metadata(directory.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o700,
        "the extraction root must be owner-only"
    );
}

#[cfg(unix)]
#[test]
fn tar_root_entry_cannot_widen_release_extraction_directory() {
    use std::os::unix::fs::PermissionsExt;

    let parent = tempfile::tempdir().unwrap();
    let archive_path = parent.path().join("release.tar");
    let destination = parent.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();
    restrict_extraction_directory(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut archive = tar::Builder::new(file);
    let archived_root = parent.path().join("archived-root");
    std::fs::create_dir(&archived_root).unwrap();
    std::fs::set_permissions(&archived_root, std::fs::Permissions::from_mode(0o775)).unwrap();
    archive.append_dir("./", &archived_root).unwrap();
    archive.finish().unwrap();

    extract_private("release.tar", &archive_path, &destination).unwrap();

    assert_eq!(
        std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o700,
        "archive root metadata must not widen the extraction directory"
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

#[cfg(unix)]
#[test]
/// Rejects tar symlink entries the same way the zip branch does.
fn tar_archives_refuse_symlink_entries() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("m.tar.gz");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "libx_module.dylib", "/outside.dylib")
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let result = extract("m.tar.gz", &archive_path, &destination);
    let link = destination.join("libx_module.dylib");
    assert!(
        result.is_err(),
        "tar symlink accepted; link exists={} -> {:?}",
        link.symlink_metadata().is_ok(),
        std::fs::read_link(&link).ok()
    );
    assert!(link.symlink_metadata().is_err());
}

#[test]
/// Rejects tar hard-link entries, which can also alias a file outside the archive.
fn tar_archives_refuse_hard_link_entries() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("m.tar");
    let destination = directory.path().join("extracted");
    std::fs::create_dir(&destination).unwrap();

    let file = std::fs::File::create(&archive_path).unwrap();
    let mut builder = tar::Builder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(0o644);
    builder
        .append_link(&mut header, "libx_module.dylib", "../outside.dylib")
        .unwrap();
    builder.finish().unwrap();

    let error = extract("m.tar", &archive_path, &destination).unwrap_err();
    assert!(
        error.to_string().contains("unsupported entry type"),
        "{error}"
    );
    assert!(
        destination
            .join("libx_module.dylib")
            .symlink_metadata()
            .is_err()
    );
}

/// Serves one archive request: headers announcing 20 bytes, then `sent` bytes
/// one every 50 ms, then silence for two seconds before closing.
fn slow_archive_server(sent: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/m.tar.gz", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n")
            .unwrap();
        for _ in 0..sent {
            std::thread::sleep(Duration::from_millis(50));
            if stream.write_all(b"x").is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });
    (url, server)
}

#[test]
/// The body budget is an idle timeout: a body that keeps arriving (one byte
/// every 50 ms, about one second in all) outlives a 300 ms stall budget.
fn a_slow_but_progressing_download_is_not_cut_off() {
    let (url, server) = slow_archive_server(20);
    let directory = tempfile::tempdir().unwrap();
    let dest = directory.path().join("m.tar.gz");
    let started = std::time::Instant::now();
    let result = download(&agent_with(Duration::from_millis(300)), &url, &dest);
    let elapsed = started.elapsed();
    let _ = server.join();
    assert!(result.is_ok(), "after {elapsed:?}: {:?}", result.err());
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 20);
}

#[test]
/// A transfer that stops delivering fails within the stall budget rather than
/// waiting for the server to give up.
fn a_stalled_download_fails_within_the_stall_budget() {
    let (url, server) = slow_archive_server(5);
    let directory = tempfile::tempdir().unwrap();
    let dest = directory.path().join("m.tar.gz");
    let started = std::time::Instant::now();
    let result = download(&agent_with(Duration::from_millis(300)), &url, &dest);
    let elapsed = started.elapsed();
    assert!(result.is_err(), "a stalled body was accepted");
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    let _ = server.join();
}
