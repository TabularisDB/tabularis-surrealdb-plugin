//! SurrealQL identifier quoting.
//!
//! SurrealQL escapes identifiers with backticks (or `⟨…⟩`). Inside a
//! backtick identifier a backtick or backslash is escaped with a backslash,
//! not by doubling it as in MySQL.

/// Quote an identifier with backticks, escaping `` ` `` and `\`.
///
/// ```
/// use surrealdb_plugin::utils::identifiers::quote_identifier;
/// assert_eq!(quote_identifier("person"), "`person`");
/// assert_eq!(quote_identifier("odd table"), "`odd table`");
/// assert_eq!(quote_identifier("a`b"), "`a\\`b`");
/// ```
pub fn quote_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('`');
    for c in name.chars() {
        if c == '`' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('`');
    out
}

/// True when `name` can be written without quotes: ASCII letters, digits and
/// `_`, not starting with a digit. Conservative on purpose — quoting a name
/// that did not need it is always valid, the reverse is not.
pub fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Write `name` plain when that is safe, quoted otherwise.
pub fn format_identifier(name: &str) -> String {
    if is_plain_identifier(name) {
        name.to_string()
    } else {
        quote_identifier(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(quote_identifier("users"), "`users`");
        assert_eq!(quote_identifier("a`b"), "`a\\`b`");
        assert_eq!(quote_identifier("back\\slash"), "`back\\\\slash`");
        assert_eq!(quote_identifier(""), "``");
        assert_eq!(quote_identifier("ünïcode"), "`ünïcode`");
    }

    #[test]
    fn plain_identifier_rules() {
        assert!(is_plain_identifier("person"));
        assert!(is_plain_identifier("_private2"));
        assert!(!is_plain_identifier("2fast"));
        assert!(!is_plain_identifier("123"));
        assert!(!is_plain_identifier("odd table"));
        assert!(!is_plain_identifier("weird-id"));
        assert!(!is_plain_identifier(""));
    }

    #[test]
    fn formats_only_when_needed() {
        assert_eq!(format_identifier("alice"), "alice");
        assert_eq!(format_identifier("weird-id"), "`weird-id`");
    }
}
