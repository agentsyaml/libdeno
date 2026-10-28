//! Random tokens and constant-time comparison.
//!
//! Used for child-mode auth, supervisor handshakes, broker socket names and
//! in-memory entry names. Every token here authenticates a peer or separates
//! two concurrent runs, so comparisons must not leak length or content
//! through timing.

/// Lowercase hex from `bytes` bytes of OS randomness.
pub(crate) fn random_hex(bytes: usize) -> Result<String, getrandom::Error> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Compares two byte strings without an early exit. An empty candidate never
/// matches: every token this module generates is non-empty, so an empty value
/// can only come from an unset or hand-rolled environment variable.
pub(crate) fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let mut difference = (candidate.len() != expected.len()) as u8;
    for (index, byte) in expected.iter().enumerate() {
        difference |= byte ^ candidate.get(index).copied().unwrap_or_default();
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_sized() {
        let token = random_hex(16).unwrap();
        assert_eq!(token.len(), 32);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(token, random_hex(16).unwrap());
    }

    #[test]
    fn comparison_rejects_empty_and_length_mismatch() {
        let token = random_hex(16).unwrap();
        assert!(constant_time_eq(token.as_bytes(), token.as_bytes()));
        assert!(!constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", token.as_bytes()));
        assert!(!constant_time_eq(b"abc", token.as_bytes()));
        let mut flipped = token.clone().into_bytes();
        flipped[0] ^= 1;
        assert!(!constant_time_eq(&flipped, token.as_bytes()));
    }
}
