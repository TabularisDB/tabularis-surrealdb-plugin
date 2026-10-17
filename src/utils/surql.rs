//! A small, conservative SurrealQL scanner.
//!
//! The plugin never parses SurrealQL for real: the server does that. It only
//! needs to answer a few structural questions about a query string, and must
//! not be fooled by strings, comments or escaped identifiers while doing so:
//!
//! - where the top-level statements are (`;` outside any brackets),
//! - which keywords appear at the top level of a statement (to decide whether
//!   `LIMIT … START …` can be appended for pagination),
//! - the first keyword of a statement (SELECT, USE, …).
//!
//! Anything it does not recognise is treated as opaque code, which keeps the
//! callers on the safe path (no pagination push-down, no rewriting).

use std::ops::Range;

/// A word that appears outside strings, comments and brackets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelWord {
    /// Upper-cased text, for case-insensitive keyword comparison.
    pub upper: String,
    pub range: Range<usize>,
}

/// One scanned statement: its text, its byte range in the source, and its
/// top-level words in order (ranges relative to `text`).
#[derive(Debug, Clone)]
pub struct Statement<'a> {
    pub text: &'a str,
    pub range: Range<usize>,
    pub words: Vec<TopLevelWord>,
}

impl Statement<'_> {
    /// The first top-level word, upper-cased (e.g. `SELECT`), if any.
    pub fn first_keyword(&self) -> Option<&str> {
        self.words.first().map(|w| w.upper.as_str())
    }

    pub fn has_top_level_keyword(&self, keyword: &str) -> bool {
        self.words.iter().any(|w| w.upper == keyword)
    }
}

/// Split a query into its top-level statements. Empty and comment-only
/// statements are dropped; the returned text is trimmed and has no trailing
/// `;`.
pub fn split_statements(sql: &str) -> Vec<Statement<'_>> {
    let mut out = Vec::new();
    let mut scanner = Scanner::new(sql);
    let mut start = 0;
    let mut words: Vec<TopLevelWord> = Vec::new();
    let mut has_code = false;

    while let Some(event) = scanner.next_event() {
        match event {
            Event::Semicolon(at) => {
                push_statement(sql, start..at, &mut words, has_code, &mut out);
                start = at + 1;
                has_code = false;
            }
            Event::Word(range) => {
                has_code = true;
                if scanner.depth == 0 && !scanner.last_word_is_member() {
                    words.push(TopLevelWord {
                        upper: sql[range.clone()].to_ascii_uppercase(),
                        range,
                    });
                }
            }
            Event::Code => has_code = true,
        }
    }
    push_statement(sql, start..sql.len(), &mut words, has_code, &mut out);
    out
}

fn push_statement<'a>(
    sql: &'a str,
    range: Range<usize>,
    words: &mut Vec<TopLevelWord>,
    has_code: bool,
    out: &mut Vec<Statement<'a>>,
) {
    let taken = std::mem::take(words);
    if !has_code {
        return;
    }
    let raw = &sql[range.clone()];
    let leading = raw.len() - raw.trim_start().len();
    let text = raw.trim();
    // Re-base word ranges onto the trimmed statement text.
    let offset = range.start + leading;
    let words = taken
        .into_iter()
        .map(|w| TopLevelWord {
            upper: w.upper,
            range: (w.range.start - offset)..(w.range.end - offset),
        })
        .collect();
    out.push(Statement {
        text,
        range: offset..offset + text.len(),
        words,
    });
}

/// Keywords that, when present at the top level of a SELECT, mean the plugin
/// must not append its own `LIMIT … START …` clause: either the user already
/// paginates, or the clause would have to go before them.
const NO_PUSHDOWN_KEYWORDS: &[&str] = &[
    "LIMIT",
    "START",
    "FETCH",
    "TIMEOUT",
    "PARALLEL",
    "TEMPFILES",
    "EXPLAIN",
    "ONLY",
];

/// True when `LIMIT n START m` can be appended to this statement as-is.
/// A statement with unbalanced brackets is a piece of a block that the
/// host's splitter cut (`… { LET $a = 1` / `SELECT … }`): a clause after it
/// would land inside or after the block, so it is never paginated.
pub fn can_push_down_pagination(statement: &Statement<'_>) -> bool {
    statement.first_keyword() == Some("SELECT")
        && is_balanced(statement.text)
        && !NO_PUSHDOWN_KEYWORDS
            .iter()
            .any(|k| statement.has_top_level_keyword(k))
}

/// Append a pagination clause, and a `TIMEOUT` when one is configured (it
/// follows `START` in the SELECT grammar). The caller checks
/// [`can_push_down_pagination`] first. The clause goes on a new line so a
/// trailing `--` comment cannot swallow it.
pub fn with_pagination(
    statement: &str,
    limit: u64,
    start: u64,
    timeout_secs: Option<u64>,
) -> String {
    match timeout_secs {
        Some(t) => format!("{statement}\nLIMIT {limit} START {start} TIMEOUT {t}s"),
        None => format!("{statement}\nLIMIT {limit} START {start}"),
    }
}

/// Undo the places where the host's generic table-browse SQL is not valid
/// (or not meaningful) SurrealQL, and leave every other query untouched:
///
/// 1. `SELECT * FROM (…) AS limited_subset` — the host wraps a user LIMIT in
///    a derived table with an alias; SurrealQL subqueries take no alias.
/// 2. `` `a``b` `` — the host escapes a backtick inside a quoted identifier by
///    doubling it; SurrealQL escapes it as `` \` ``.
/// 3. ``SELECT * FROM `t` WHERE `id` = 't:x'`` — the host's foreign-key
///    lookup compares the record id with a string, which never matches in
///    SurrealQL; the string is cast with `<record>`.
pub fn normalize_host_browse_query(query: &str) -> String {
    let normalized = normalize_browse_shapes(query);
    rewrite_record_id_lookup(&normalized).unwrap_or(normalized)
}

fn normalize_browse_shapes(query: &str) -> String {
    let trimmed = query.trim().trim_end_matches(';').trim_end();
    const WRAP_PREFIX: &str = "SELECT * FROM (";
    const WRAP_SUFFIX: &str = ") AS limited_subset";
    if starts_with_ignore_case(trimmed, WRAP_PREFIX) && ends_with_ignore_case(trimmed, WRAP_SUFFIX)
    {
        let inner = &trimmed[WRAP_PREFIX.len()..trimmed.len() - WRAP_SUFFIX.len()];
        if is_balanced(inner) {
            return format!("SELECT * FROM ({})", normalize_browse_shapes(inner));
        }
    }
    const BROWSE_PREFIX: &str = "SELECT * FROM `";
    if starts_with_ignore_case(trimmed, BROWSE_PREFIX) {
        if let Some(rewritten) = rewrite_doubled_backtick_table(trimmed, BROWSE_PREFIX.len() - 1) {
            return rewritten;
        }
    }
    trimmed.to_string()
}

/// Rewrite the backtick identifier starting at `open` (the index of its
/// opening backtick) from host escaping (doubled backticks) to SurrealQL
/// escaping. Returns `None` when there is nothing to rewrite.
fn rewrite_doubled_backtick_table(sql: &str, open: usize) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = open + 1;
    let mut name = String::new();
    let mut doubled = false;
    while i < bytes.len() {
        match bytes[i] {
            b'`' if bytes.get(i + 1) == Some(&b'`') => {
                doubled = true;
                name.push('`');
                i += 2;
            }
            b'`' => {
                if !doubled {
                    return None;
                }
                let rest = &sql[i + 1..];
                return Some(format!(
                    "{}{}{}",
                    &sql[..open],
                    crate::utils::identifiers::quote_identifier(&name),
                    rest
                ));
            }
            b'\\' => return None, // already SurrealQL-escaped
            _ => {
                let ch = sql[i..].chars().next()?;
                name.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    None
}

/// ``SELECT * FROM <table> WHERE <id> = '<table>:…'`` (the whole statement)
/// → ``SELECT * FROM <table> WHERE id = <record> '<table>:…'``. Only rewritten
/// when the string starts with the same table, so the cast cannot fail on
/// unrelated text. The value stays a string literal: no injection.
fn rewrite_record_id_lookup(sql: &str) -> Option<String> {
    let rest = strip_prefix_ignore_case(sql, "SELECT * FROM ")?;
    let (table, after_table) = parse_identifier(rest)?;
    let table_sql = &rest[..rest.len() - after_table.len()];
    let rest = strip_prefix_ignore_case(after_table.trim_start(), "WHERE ")?;
    let (column, rest) = parse_identifier(rest.trim_start())?;
    if column != "id" {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, rest) = parse_single_quoted(rest)?;
    if !rest.trim().is_empty() {
        return None;
    }
    let prefix = format!("{}:", crate::utils::identifiers::format_identifier(&table));
    if !value.starts_with(&prefix) || value.len() == prefix.len() {
        return None;
    }
    Some(format!(
        "SELECT * FROM {table_sql} WHERE id = <record> {}",
        crate::utils::values::quote_string(&value)
    ))
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    starts_with_ignore_case(s, prefix).then(|| &s[prefix.len()..])
}

/// A backtick-quoted (`\` escapes or doubled backticks) or plain identifier.
fn parse_identifier(s: &str) -> Option<(String, &str)> {
    if let Some(body) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => name.push(chars.next()?.1),
                '`' if body[i + 1..].starts_with('`') => {
                    chars.next();
                    name.push('`');
                }
                '`' => return Some((name, &body[i + 1..])),
                other => name.push(other),
            }
        }
        return None;
    }
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// A `'…'` literal, accepting both `''` and `\'` escapes.
fn parse_single_quoted(s: &str) -> Option<(String, &str)> {
    let body = s.strip_prefix('\'')?;
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => value.push(chars.next()?.1),
            '\'' if body[i + 1..].starts_with('\'') => {
                chars.next();
                value.push('\'');
            }
            '\'' => return Some((value, &body[i + 1..])),
            other => value.push(other),
        }
    }
    None
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.is_char_boundary(s.len() - suffix.len())
        && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// True when brackets in `sql` (outside strings and comments) balance and
/// never close below zero.
/// True when `sql` is one balanced expression with no second statement, so
/// it can be embedded in another statement as `WHERE (<sql>\n)` without
/// changing that statement's shape (used for the host's table filter).
pub fn is_single_expression(sql: &str) -> bool {
    is_balanced(sql) && split_statements(sql).len() <= 1
}

pub fn is_balanced(sql: &str) -> bool {
    let mut scanner = Scanner::new(sql);
    while scanner.next_event().is_some() {
        if scanner.underflow {
            return false;
        }
    }
    scanner.depth == 0 && !scanner.underflow && !scanner.unterminated
}

#[derive(Debug)]
enum Event {
    Semicolon(usize),
    Word(Range<usize>),
    Code,
}

/// Character-level scanner shared by the helpers above.
pub(crate) struct Scanner<'a> {
    src: &'a str,
    pos: usize,
    pub(crate) depth: usize,
    underflow: bool,
    unterminated: bool,
    /// The last significant (non-space, non-comment) character before the
    /// current word, used to tell `person:limit` or `a.start` apart from the
    /// LIMIT / START keywords.
    prev_significant: Option<char>,
    /// `prev_significant` as it was just before the most recent word.
    prev_word_context: Option<char>,
}

impl<'a> Scanner<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            depth: 0,
            underflow: false,
            unterminated: false,
            prev_significant: None,
            prev_word_context: None,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(offset)
    }

    /// True when the word just scanned follows `:`, `.` or `$`, i.e. it is a
    /// record-id part, a field path segment or a parameter name.
    fn last_word_is_member(&self) -> bool {
        matches!(self.prev_word_context, Some(':' | '.' | '$'))
    }

    fn next_event(&mut self) -> Option<Event> {
        loop {
            let ch = self.peek()?;
            let start = self.pos;
            match ch {
                c if c.is_whitespace() => {
                    self.pos += c.len_utf8();
                }
                '-' if self.peek_at(1) == Some('-') => self.skip_line(),
                '/' if self.peek_at(1) == Some('/') => self.skip_line(),
                '#' => self.skip_line(),
                '/' if self.peek_at(1) == Some('*') => self.skip_block_comment(),
                '\'' | '"' | '`' => {
                    self.skip_quoted(ch);
                    self.mark_significant(ch);
                    return Some(Event::Code);
                }
                '⟨' => {
                    self.skip_quoted('⟩');
                    self.mark_significant('⟩');
                    return Some(Event::Code);
                }
                ';' if self.depth == 0 => {
                    self.pos += 1;
                    self.prev_significant = None;
                    return Some(Event::Semicolon(start));
                }
                '(' | '[' | '{' => {
                    self.depth += 1;
                    self.pos += 1;
                    self.mark_significant(ch);
                    return Some(Event::Code);
                }
                ')' | ']' | '}' => {
                    if self.depth == 0 {
                        self.underflow = true;
                    } else {
                        self.depth -= 1;
                    }
                    self.pos += 1;
                    self.mark_significant(ch);
                    return Some(Event::Code);
                }
                c if c.is_alphanumeric() || c == '_' => {
                    while let Some(c) = self.peek() {
                        if c.is_alphanumeric() || c == '_' {
                            self.pos += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    self.prev_word_context = self.prev_significant;
                    self.mark_significant('a');
                    return Some(Event::Word(start..self.pos));
                }
                c => {
                    self.pos += c.len_utf8();
                    self.mark_significant(c);
                    return Some(Event::Code);
                }
            }
        }
    }

    fn mark_significant(&mut self, c: char) {
        self.prev_significant = Some(c);
    }

    fn skip_line(&mut self) {
        match self.src[self.pos..].find('\n') {
            Some(n) => self.pos += n + 1,
            None => self.pos = self.src.len(),
        }
    }

    fn skip_block_comment(&mut self) {
        match self.src[self.pos + 2..].find("*/") {
            Some(n) => self.pos += 2 + n + 2,
            None => {
                self.unterminated = true;
                self.pos = self.src.len();
            }
        }
    }

    /// Skip a quoted run that starts at the current position and ends at
    /// `close`, honouring backslash escapes.
    fn skip_quoted(&mut self, close: char) {
        let open = self.peek().unwrap_or(close);
        self.pos += open.len_utf8();
        while let Some(c) = self.peek() {
            self.pos += c.len_utf8();
            if c == '\\' {
                if let Some(next) = self.peek() {
                    self.pos += next.len_utf8();
                }
            } else if c == close {
                return;
            }
        }
        self.unterminated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split_statements(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_top_level_semicolons_only() {
        assert_eq!(
            texts("SELECT * FROM a; SELECT ';' FROM b ;; "),
            vec!["SELECT * FROM a", "SELECT ';' FROM b"]
        );
        assert_eq!(
            texts("DEFINE FUNCTION fn::x() { LET $a = 1; RETURN $a; }; RETURN fn::x();"),
            vec![
                "DEFINE FUNCTION fn::x() { LET $a = 1; RETURN $a; }",
                "RETURN fn::x()"
            ]
        );
    }

    #[test]
    fn ignores_semicolons_in_comments_identifiers_and_escaped_strings() {
        let sql = "-- a; b\nSELECT `x;y`, ⟨p;q⟩, \"it\\\"s;\" FROM t /* ; */ # ;\n; // tail;";
        assert_eq!(
            texts(sql),
            vec!["-- a; b\nSELECT `x;y`, ⟨p;q⟩, \"it\\\"s;\" FROM t /* ; */ # ;"]
        );
    }

    #[test]
    fn drops_comment_only_statements() {
        assert!(texts("-- nothing here\n/* or here */ ;").is_empty());
    }

    #[test]
    fn reports_top_level_words_only() {
        let stmts = split_statements("SELECT (SELECT * FROM b LIMIT 1) FROM a WHERE x = 'LIMIT'");
        let words: Vec<_> = stmts[0].words.iter().map(|w| w.upper.as_str()).collect();
        assert_eq!(words, vec!["SELECT", "FROM", "A", "WHERE", "X"]);
    }

    #[test]
    fn record_ids_and_fields_named_like_keywords_are_not_keywords() {
        let stmt = &split_statements("SELECT a.start FROM person:limit")[0];
        assert!(!stmt.has_top_level_keyword("START"));
        assert!(!stmt.has_top_level_keyword("LIMIT"));
        assert!(can_push_down_pagination(stmt));
    }

    #[test]
    fn pagination_push_down_rules() {
        let check = |sql: &str| can_push_down_pagination(&split_statements(sql)[0]);
        assert!(check("SELECT * FROM `person` WHERE age > 3 ORDER BY name"));
        assert!(check("select ->knows->person AS friends from person"));
        assert!(!check("SELECT * FROM person LIMIT 5"));
        assert!(!check("SELECT * FROM person START 5"));
        assert!(!check("SELECT * FROM person FETCH friends"));
        assert!(!check("SELECT * FROM ONLY person:alice"));
        assert!(!check("SELECT * FROM person EXPLAIN"));
        assert!(!check("RETURN 1"));
        assert!(!check("CREATE person CONTENT {}"));
        assert!(check("SELECT * FROM (SELECT * FROM person LIMIT 50)"));
        // Pieces of a block the host's splitter cut.
        assert!(!check("SELECT * FROM item WHERE n >= $m }"));
        assert!(!check("SELECT * FROM { LET $a = 1"));
    }

    #[test]
    fn appends_limit_then_start() {
        assert_eq!(
            with_pagination("SELECT * FROM `person`", 101, 200, None),
            "SELECT * FROM `person`\nLIMIT 101 START 200"
        );
        let stmt = &split_statements("SELECT * FROM person -- newest first")[0];
        assert!(can_push_down_pagination(stmt));
        assert_eq!(
            with_pagination(stmt.text, 2, 0, None),
            "SELECT * FROM person -- newest first\nLIMIT 2 START 0"
        );
    }

    #[test]
    fn unwraps_host_limit_subquery_alias() {
        assert_eq!(
            normalize_host_browse_query(
                "SELECT * FROM (SELECT * FROM `person` WHERE age > 1 LIMIT 50) AS limited_subset"
            ),
            "SELECT * FROM (SELECT * FROM `person` WHERE age > 1 LIMIT 50)"
        );
        // Not the host shape: untouched.
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM (SELECT 1) AS x"),
            "SELECT * FROM (SELECT 1) AS x"
        );
    }

    #[test]
    fn rewrites_host_doubled_backticks_in_browse_table() {
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM `we``ird` LIMIT 10"),
            "SELECT * FROM `we\\`ird` LIMIT 10"
        );
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM `plain table`;"),
            "SELECT * FROM `plain table`"
        );
        assert_eq!(
            normalize_host_browse_query(
                "SELECT * FROM (SELECT * FROM `a``b` LIMIT 2) AS limited_subset"
            ),
            "SELECT * FROM (SELECT * FROM `a\\`b` LIMIT 2)"
        );
    }

    #[test]
    fn appends_a_timeout_after_start() {
        assert_eq!(
            with_pagination("SELECT * FROM t", 11, 0, Some(30)),
            "SELECT * FROM t\nLIMIT 11 START 0 TIMEOUT 30s"
        );
    }

    #[test]
    fn statement_ranges_point_into_the_source() {
        let sql = "  RETURN 1 ;\n SELECT * FROM t";
        let stmts = split_statements(sql);
        assert_eq!(&sql[stmts[0].range.clone()], "RETURN 1");
        assert_eq!(&sql[stmts[1].range.clone()], "SELECT * FROM t");
    }

    #[test]
    fn rewrites_host_foreign_key_lookups() {
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM `person` WHERE `id` = 'person:alice'"),
            "SELECT * FROM `person` WHERE id = <record> 'person:alice'"
        );
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM `event` WHERE `id` = 'event:`weird-id`'"),
            "SELECT * FROM `event` WHERE id = <record> 'event:`weird-id`'"
        );
        assert_eq!(
            normalize_host_browse_query(
                "SELECT * FROM `event` WHERE `id` = 'event:[2024, ''q1'']'"
            ),
            "SELECT * FROM `event` WHERE id = <record> 'event:[2024, \\'q1\\']'"
        );
        assert_eq!(
            normalize_host_browse_query("SELECT * FROM `odd table` WHERE id = '`odd table`:x'"),
            "SELECT * FROM `odd table` WHERE id = <record> '`odd table`:x'"
        );
        // Not a record id of that table, another column, or more clauses:
        // untouched.
        for q in [
            "SELECT * FROM `person` WHERE `id` = 'alice'",
            "SELECT * FROM `person` WHERE `id` = 'event:1'",
            "SELECT * FROM `person` WHERE `name` = 'person:alice'",
            "SELECT * FROM `person` WHERE `id` = 'person:alice' AND age > 3",
        ] {
            assert_eq!(normalize_host_browse_query(q), q);
        }
    }

    #[test]
    fn leaves_other_queries_untouched() {
        let q = "SELECT * FROM person WHERE name = 'x'";
        assert_eq!(normalize_host_browse_query(q), q);
    }

    #[test]
    fn balance_check() {
        assert!(is_balanced("SELECT (1) FROM [a]"));
        assert!(!is_balanced("SELECT 1) FROM (2"));
        assert!(!is_balanced("SELECT ')"));
    }

    #[test]
    fn single_expressions() {
        assert!(is_single_expression("n > 2 AND s = 'a;b'"));
        assert!(is_single_expression("`odd;name` = 1 -- trailing comment"));
        assert!(is_single_expression(""));
        assert!(!is_single_expression("1; DELETE item"));
        assert!(!is_single_expression("1) OR (1"));
        assert!(!is_single_expression("s = 'open"));
        assert!(!is_single_expression("n > 1 /* unterminated"));
    }
}
