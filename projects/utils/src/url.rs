//! URL percent-encoding — the one place in the workspace that knows how orca
//! escapes a string for use in a URL path segment or query value. **Every
//! callsite that used to inline `urlencoding::…` should call through here.**
//! The backing library is hidden: no caller names it. This is an abstraction,
//! not a re-export.

/// Percent-encode `s` for safe inclusion in a URL path segment or query value
/// (spaces → `%20`, `/` → `%2F`, etc.). Returns an owned `String`.
pub fn encode(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// Reverse [`encode`] — decode percent-escapes back to their bytes as a UTF-8
/// string. Errors if the result is not valid UTF-8.
pub fn decode(s: &str) -> Result<String, std::string::FromUtf8Error> {
    urlencoding::decode(s).map(|c| c.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn round_trips() {
        let raw = "name=value & more/stuff";
        assert_eq!(decode(&encode(raw)).unwrap(), raw);
    }
}
