//! Framing: a 4-byte big-endian length, then that many bytes of JSON.
//!
//! Newline-delimited JSON would have been shorter to write and is what most
//! JSON-RPC-over-socket code does. It is the wrong choice here for one reason:
//! a body can legitimately contain a newline (a transcript, a mail body, an
//! error backtrace), and the encoder would then have to escape it, meaning the
//! frame boundary depends on the *content* of the payload. A length prefix
//! makes reading a frame a fixed-cost operation that cannot be confused by
//! anything a caller puts in the body, and it lets the reader reject an
//! oversized frame before allocating for it.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// The largest frame the reader will allocate for.
///
/// A hard cap, not a tunable. The frame length arrives from the wire *before*
/// the bytes do, so without this the first four bytes of a hostile or corrupt
/// stream are a 4 GiB allocation. 16 MiB is far above any legitimate control
/// message; a payload that does not fit goes through [`crate::stream`], which
/// splits it into chunks that do, rather than through a larger cap here.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// The length prefix's width, in bytes.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// Encode `value` into a length-prefixed frame.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(Error::protocol(format!(
            "frame of {} bytes exceeds the {MAX_FRAME_LEN}-byte cap",
            payload.len()
        )));
    }
    let mut frame = Vec::with_capacity(LENGTH_PREFIX_LEN + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Read the payload length out of a length prefix, rejecting oversized frames.
pub fn decode_length(prefix: [u8; LENGTH_PREFIX_LEN]) -> Result<usize> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_LEN {
        return Err(Error::protocol(format!(
            "peer announced a {len}-byte frame, over the {MAX_FRAME_LEN}-byte cap"
        )));
    }
    Ok(len)
}

/// Decode a payload that has already been read in full.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(payload)?)
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
