/// Returns a prefix of at most `max_bytes` without splitting a UTF-8 codepoint,
/// plus whether any input was omitted.
#[must_use]
pub fn truncate_utf8(input: &str, max_bytes: usize) -> (&str, bool) {
    if input.len() <= max_bytes {
        return (input, false);
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    (&input[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_ascii_and_exact_limits() {
        assert_eq!(truncate_utf8("hello", 10), ("hello", false));
        assert_eq!(truncate_utf8("hello", 5), ("hello", false));
        assert_eq!(truncate_utf8("hello", 3), ("hel", true));
    }

    #[test]
    fn never_splits_multibyte_characters() {
        assert_eq!(truncate_utf8("aé🙂z", 0), ("", true));
        assert_eq!(truncate_utf8("aé🙂z", 2), ("a", true));
        assert_eq!(truncate_utf8("aé🙂z", 3), ("aé", true));
        assert_eq!(truncate_utf8("aé🙂z", 6), ("aé", true));
        assert_eq!(truncate_utf8("aé🙂z", 7), ("aé🙂", true));
    }
}
