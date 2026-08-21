//! Tiny shell-quoting helper shared by the scheduler and fs commands.

/// POSIX single-quote an argument: wrap in `'…'`, escaping embedded
/// single quotes as `'\''`. Safe for any byte sequence except NUL.
pub fn quote_arg(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quotes() {
        assert_eq!(quote_arg("hello"), "'hello'");
        assert_eq!(quote_arg("it's"), "'it'\\''s'");
        assert_eq!(quote_arg(""), "''");
    }
}
