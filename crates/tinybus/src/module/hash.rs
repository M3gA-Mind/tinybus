//! SHA-256 used by module artifact verification.
//!
//! Verification at load is what an attestation later stands on: a recipient
//! earns the right to be handed a confidential body by having its artifact
//! digested here and matched against the operator's allowlist. Compiled
//! unconditionally rather than behind `modules`, because `module::sha256_file`
//! offers the same digest to callers that publish an asset without ever
//! loading one.
//!
//! Two things keep this off the startup path. Input is read in large chunks,
//! not one 64-byte block per `read` syscall, and the compression function is
//! `sha2`'s, which uses the CPU's SHA extensions where it has them. And a
//! digest of an allowlisted artifact is remembered for the life of the process,
//! keyed on the identity of the open file (see [`file_hex_remembered`]), so the
//! cache check and the load gate do not each pay for the same 20 MB library.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

/// Bytes requested per `read`. Large enough that a 25 MB library is ~100
/// syscalls, small enough to stay out of the allocator's way.
const CHUNK: usize = 256 * 1024;

/// Lowercase hex SHA-256 of everything `reader` yields.
pub(crate) fn file_hex(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => hasher.update(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// [`file_hex`] over an open file, remembering the answer for this process.
///
/// A file is the same file only if its device, inode, length, modification
/// time and status-change time all match what they were when it was hashed.
/// The status-change time moves on every write and cannot be set from user
/// space, so a library rewritten in place does not match, even if its length
/// and modification time were put back. All of it is read from the open handle
/// being hashed, never from a path, so it cannot describe a different file
/// than the bytes read.
///
/// Nothing is remembered for a file touched within the last
/// [`REMEMBER_AFTER`], because a write in the same clock tick as the hash would
/// leave the identity unchanged. Platforms without the identity fields hash
/// every time.
pub(crate) fn file_hex_remembered(file: std::fs::File) -> io::Result<String> {
    remembered::hex(file, remembered::REMEMBER_AFTER).map(|(hex, _)| hex)
}

#[cfg(unix)]
mod remembered {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub(super) const REMEMBER_AFTER: Duration = Duration::from_secs(2);

    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Identity {
        dev: u64,
        ino: u64,
        len: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }

    fn identity(file: &File) -> io::Result<Identity> {
        let meta = file.metadata()?;
        Ok(Identity {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    fn quiet_for(identity: &Identity, age: Duration) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        let newest = identity.mtime.0.max(identity.ctime.0);
        u64::try_from(newest).is_ok_and(|newest| Duration::from_secs(newest) + age <= now)
    }

    fn memory() -> &'static Mutex<HashMap<Identity, String>> {
        static MEMORY: OnceLock<Mutex<HashMap<Identity, String>>> = OnceLock::new();
        MEMORY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// The digest, and whether it was served from memory.
    pub(super) fn hex(file: File, age: Duration) -> io::Result<(String, bool)> {
        let before = identity(&file)?;
        let eligible = quiet_for(&before, age);
        if eligible && let Some(known) = memory().lock().ok().and_then(|m| m.get(&before).cloned())
        {
            return Ok((known, true));
        }
        let digest = super::file_hex(&file)?;
        // Hashed bytes are only attributable to this identity if the file did
        // not change underneath the read.
        if eligible
            && identity(&file).is_ok_and(|after| after == before)
            && let Ok(mut memory) = memory().lock()
        {
            memory.insert(before, digest.clone());
        }
        Ok((digest, false))
    }
}

#[cfg(not(unix))]
mod remembered {
    use std::io;
    use std::time::Duration;

    pub(super) const REMEMBER_AFTER: Duration = Duration::from_secs(2);

    pub(super) fn hex(file: std::fs::File, _age: Duration) -> io::Result<(String, bool)> {
        super::file_hex(file).map(|digest| (digest, false))
    }
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
