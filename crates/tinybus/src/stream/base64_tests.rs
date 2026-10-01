use super::*;

#[test]
fn every_length_modulo_three_round_trips() {
    for len in 0..=64usize {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let text = encode(&bytes);
        assert_eq!(decode(&text).unwrap(), bytes, "len {len}");
    }
}

#[test]
fn the_encoding_matches_the_rfc_test_vectors() {
    assert_eq!(encode(b""), "");
    assert_eq!(encode(b"f"), "Zg==");
    assert_eq!(encode(b"fo"), "Zm8=");
    assert_eq!(encode(b"foo"), "Zm9v");
    assert_eq!(encode(b"foob"), "Zm9vYg==");
    assert_eq!(encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(encode(b"foobar"), "Zm9vYmFy");
}

#[test]
fn the_full_byte_range_survives_a_round_trip() {
    let bytes: Vec<u8> = (0..=255u8).collect();
    assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
}

#[test]
fn a_truncated_group_is_rejected_rather_than_padded_silently() {
    assert!(decode("Zm9").is_err());
}

#[test]
fn a_symbol_outside_the_alphabet_is_rejected() {
    assert!(decode("Zm9*").is_err());
    assert!(decode("Zm9 ").is_err());
}

#[test]
fn padding_in_the_middle_of_a_chunk_is_rejected() {
    assert!(decode("Zg==Zg==").is_err());
    assert!(decode("=g==").is_err());
}

#[test]
fn a_padded_group_carrying_bits_it_does_not_encode_is_rejected() {
    // "Zh==" decodes the same byte as "Zg==" under a lenient decoder.
    assert!(decode("Zh==").is_err());
    assert!(decode("Zm9=").is_err());
}

#[test]
fn a_decode_failure_never_quotes_the_chunk_it_rejected() {
    let secret = "recovery-phrase!";
    let error = decode(secret).unwrap_err().to_string();
    assert!(!error.contains(secret), "{error}");
}
