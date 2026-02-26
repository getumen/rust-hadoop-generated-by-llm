/// URI encoding according to SigV4 requirements.
/// S3-specific rules:
/// - Keep A-Z, a-z, 0-9, _, ., -, ~
/// - Percent-encode everything else with uppercase hex.
/// - Space is encoded as %20.
/// - Slash (/) handling depends on whether it's path or query.
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut result = String::with_capacity(input.len());
    for b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                result.push(*b as char);
            }
            b'/' => {
                if encode_slash {
                    result.push_str("%2F");
                } else {
                    result.push('/');
                }
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_encode() {
        assert_eq!(uri_encode("abc", true), "abc");
        assert_eq!(uri_encode("a b c", true), "a%20b%20c");
        assert_eq!(uri_encode("a/b/c", false), "a/b/c");
        assert_eq!(uri_encode("a/b/c", true), "a%2Fb%2Fc");
        assert_eq!(uri_encode("-._~", true), "-._~");
        assert_eq!(uri_encode("あ", true), "%E3%81%82"); // UTF-8 "あ"
    }
}
