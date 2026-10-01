use super::*;

#[test]
fn a_secret_round_trips_the_bytes_it_was_built_from() {
    let secret = Secret::new(vec![1, 2, 3, 4, 5]);
    assert_eq!(secret.expose_secret(), &[1, 2, 3, 4, 5]);
    assert_eq!(secret.len(), 5);
    assert!(!secret.is_empty());
}

#[test]
fn an_empty_secret_reports_empty_without_touching_a_null_pointer() {
    let secret = Secret::new(Vec::new());
    assert!(secret.is_empty());
    assert_eq!(secret.len(), 0);
    assert_eq!(secret.expose_secret(), &[] as &[u8]);
}

#[test]
fn a_secret_never_prints_its_contents_when_debug_formatted() {
    let secret = Secret::new(b"correct horse battery staple".to_vec());
    let printed = format!("{secret:?}");
    assert!(!printed.contains("correct"));
    assert!(!printed.contains("horse"));
    assert!(!printed.contains("battery"));
    assert!(!printed.contains("staple"));
    assert_eq!(printed, "Secret([redacted], 28 bytes)");
}

#[test]
fn a_secret_never_prints_its_contents_when_display_formatted() {
    let secret = Secret::new(b"top secret payload".to_vec());
    let printed = format!("{secret}");
    assert!(!printed.contains("top"));
    assert!(!printed.contains("secret"));
    assert!(!printed.contains("payload"));
    assert_eq!(printed, "Secret([redacted], 18 bytes)");
}

#[test]
fn a_secrets_debug_output_does_not_leak_length_derived_secrets() {
    // The length itself is reported by design (it is not sensitive on its
    // own), but nothing *derived* from the bytes — a checksum, a prefix,
    // anything — should ever show up alongside it.
    let secret = Secret::new(vec![0xAB; 8]);
    let printed = format!("{secret:?}");
    assert_eq!(printed, "Secret([redacted], 8 bytes)");
}

#[test]
fn construction_succeeds_even_when_the_memory_lock_would_fail() {
    // `mlock` routinely fails under a low RLIMIT_MEMLOCK; a `Secret`
    // large enough to blow past a typical unprivileged limit still has
    // to construct successfully and hold its bytes correctly. This does
    // not assert on `locked` (there is no portable way to force the
    // syscall to fail), only that a large buffer still round-trips.
    let big = vec![0x42u8; 4 * 1024 * 1024];
    let secret = Secret::new(big.clone());
    assert_eq!(secret.expose_secret(), big.as_slice());
}

#[test]
fn zeroizing_a_live_buffer_overwrites_every_byte_with_zero() {
    // Exercises the zeroization routine directly on a buffer this test
    // still owns, rather than reading a `Secret` after it has been
    // dropped (which would be a read of freed memory and undefined
    // behaviour).
    let mut bytes = vec![1u8, 2, 3, 4, 5, 255, 128, 7];
    // SAFETY: `bytes.as_mut_ptr()` is valid for `bytes.len()` writes —
    // the `Vec`'s own guarantee.
    unsafe { zeroize_raw(bytes.as_mut_ptr(), bytes.len()) };
    assert_eq!(bytes, vec![0u8; 8]);
}

#[test]
fn zeroizing_an_empty_buffer_is_a_harmless_no_op() {
    let mut bytes: Vec<u8> = Vec::new();
    // SAFETY: `len` is `0`, so no byte is ever written; the pointer's
    // validity for zero writes is unconditional.
    unsafe { zeroize_raw(bytes.as_mut_ptr(), bytes.len()) };
    assert!(bytes.is_empty());
}

#[test]
fn zeroizing_covers_the_full_capacity_not_just_the_initialized_length() {
    // Reproduces the shape a caller's `Vec` is left in by
    // `key.truncate(32)`: `len` shrinks, `capacity` does not, and the
    // truncated tail is still sitting in the allocation. `Secret`'s
    // `Drop` must clear that tail too, not just the first `len` bytes.
    let mut bytes: Vec<u8> = Vec::with_capacity(16);
    // SAFETY: `bytes` has capacity for 16 bytes; every one of them is
    // written before `set_len` claims it is initialized, so this upholds
    // `Vec`'s invariant rather than violating it.
    unsafe {
        for i in 0..16 {
            std::ptr::write(bytes.as_mut_ptr().add(i), 0xAB);
        }
        bytes.set_len(16);
    }
    bytes.truncate(4); // len 4, capacity unchanged; bytes[4..16] still 0xAB.
    let cap = bytes.capacity();
    assert!(cap >= 16, "capacity should not shrink on truncate");

    // SAFETY: `bytes.as_mut_ptr()` is valid for `cap` bytes of writes —
    // it is the pointer to `bytes`'s own live allocation, sized exactly
    // `cap`, and `bytes` is not touched by anything else during this call.
    unsafe { zeroize_raw(bytes.as_mut_ptr(), cap) };

    // Peek at the whole allocation, including the part past `len`, to
    // confirm the spare capacity was zeroized too. This is a read of
    // memory `bytes` still owns and has not freed — unlike reading a
    // `Secret` after `Drop`, this is not use-after-free.
    // SAFETY: bytes 0..cap were all explicitly initialized above (first
    // to 0xAB, then zeroized), so claiming the full capacity as
    // initialized here is accurate.
    unsafe {
        bytes.set_len(cap);
    }
    assert_eq!(bytes, vec![0u8; cap]);
}

#[test]
fn harden_and_unlock_round_trip_without_panicking_on_a_live_allocation() {
    // Exercises the lock/unlock pair directly on a buffer this test
    // still owns and frees itself, independent of `Secret`'s `Drop`.
    // Locking may or may not succeed depending on the sandbox's
    // RLIMIT_MEMLOCK; either outcome is acceptable, only a panic is not.
    let mut bytes = vec![9u8; 4096];
    let locked = harden_buffer(bytes.as_mut_ptr(), bytes.len());
    if locked {
        unlock_buffer(bytes.as_mut_ptr(), bytes.len());
    }
}

#[test]
fn dropping_a_secret_does_not_panic_regardless_of_lock_state() {
    // The `Drop` impl's zeroize-then-maybe-unlock sequence is exercised
    // implicitly by every other test via scope exit; this test makes the
    // property explicit for both the locked and empty cases.
    {
        let _secret = Secret::new(vec![1, 2, 3]);
    }
    {
        let _secret = Secret::new(Vec::new());
    }
}
