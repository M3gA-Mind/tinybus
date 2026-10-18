use super::*;

static ASSETS: [ReleaseAsset<'static>; 2] = [
    ReleaseAsset {
        host_key: "ubuntu-24.04-x86_64",
        archive: "tinybus-linux.tar.gz",
        sha256: "abc123",
    },
    ReleaseAsset {
        host_key: "windows-2025-x86_64",
        archive: "tinybus-windows.zip",
        sha256: "def456",
    },
];

static RECORD: ReleaseRecord = ReleaseRecord {
    id: "tinybus-test",
    bus_name: "ai.tinyhumans.test.TinyBus",
    object_path: "/ai/tinyhumans/test/TinyBus",
    version: "1.0.0",
    release_url: "https://github.com/tinyhumansai/tinybus/releases/tag/v1.0.0",
    assets: &ASSETS,
};

#[test]
fn asset_lookup_matches_the_exact_host_key_and_rejects_unknown_hosts() {
    assert_eq!(RECORD.asset_for(ASSETS[0].host_key), Some(&ASSETS[0]));
    assert_eq!(RECORD.asset_for("unknown-host"), None);
}
