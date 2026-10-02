//! SHA-256 used by module artifact verification.
//!
//! Verification at load is what an attestation later stands on: a recipient
//! earns the right to be handed a confidential body by having its artifact
//! digested here and matched against the operator's allowlist. Compiled
//! unconditionally rather than behind `modules`, because `module::sha256_file`
//! offers the same digest to callers that publish an asset without ever
//! loading one.
//!
//! Input is read in large chunks, not one 64-byte block per `read` syscall, and
//! the compression function is `sha2`'s, which uses the CPU's SHA extensions
//! where it has them, so hashing a 25 MB library stays off the startup path.
//! Hashing the same library twice is avoided one level up, in
//! `remembered_hash`.

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

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
