//! A small, conservative SurrealQL scanner.
//!
//! The plugin never parses SurrealQL for real: the server does that. It only
//! needs to answer a few structural questions about a query string, and must
//! not be fooled by strings, comments or escaped identifiers while doing so:
//!
//! - where the top-level statements are (`;` outside any brackets),
//! - which keywords appear at the top level of a statement (to decide whether
//!   `LIMIT … START …` can be appended for pagination),
//! - the first keyword of a statement (SELECT, USE, …),
//! - whether a statement has another one that writes inside it (a SELECT
//!   that writes is run once, never paged),
//! - which conditions of a table filter compare a column with text (to cast
//!   record links).
//!
//! It knows strings, comments, escaped identifiers (`` `…` ``, `⟨…⟩`) and
//! regex literals (`/…/`), so a `;`, bracket or quote inside them is not
//! taken for code.
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

/// Statements that change data or the schema.
const WRITE_STATEMENT_KEYWORDS: &[&str] = &[
    "CREATE", "INSERT", "UPDATE", "UPSERT", "DELETE", "RELATE", "DEFINE", "REMOVE", "ALTER",
    "REBUILD",
];

/// True when `sql` contains a statement that writes, at any depth: in a
/// subquery (`SELECT * FROM (UPDATE t SET n += 1)`), in a block
/// (`{ CREATE t; RETURN [] }`), or after FROM without brackets
/// (`SELECT * FROM UPDATE t SET …`), which 2.x and 3.x both run. SurrealQL
/// reads these keywords as the start of a statement where an expression
/// stands (`SELECT update FROM t` does not parse), so every one counts
/// except a field path, parameter, record id or function name (`a.update`,
/// `$update`, `array::insert`) and an object key (`{ update: 1 }`). The few
/// places that take one as a name (`->update->`, `ORDER BY update`) count
/// too, which only costs that query its paging.
pub fn contains_write(sql: &str) -> bool {
    let mut scanner = Scanner::new(sql);
    while let Some(event) = scanner.next_event() {
        let Event::Word(range) = event else {
            continue;
        };
        let after = sql[range.end..].trim_start();
        let object_key = after.starts_with(':') && !after.starts_with("::");
        if !scanner.last_word_is_member()
            && !object_key
            && WRITE_STATEMENT_KEYWORDS
                .iter()
                .any(|k| sql[range.clone()].eq_ignore_ascii_case(k))
        {
            return true;
        }
    }
    false
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
/// when the string is a record id of the same table ([`record_id_table`]),
/// so the cast cannot fail on unrelated text or pick another record. The
/// value stays a string literal: no injection.
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
    if record_id_table(&value).as_deref() != Some(table.as_str()) {
        return None;
    }
    Some(format!(
        "SELECT * FROM {table_sql} WHERE id = <record> {}",
        crate::utils::values::quote_string(&value)
    ))
}

/// Top-level keywords after which a SELECT no longer returns the records of
/// its table.
const NOT_RECORDS_KEYWORDS: &[&str] = &["SPLIT", "GROUP", "EXPLAIN"];

/// The table that a query of the host's table-browse shape reads, and the
/// byte range of the SELECT that reads it: ``SELECT * FROM `t` `` with only
/// the WHERE, ORDER BY and LIMIT clauses the host adds, also inside its
/// LIMIT wrapper (`SELECT * FROM (…)` after [`normalize_host_browse_query`]).
/// The host quotes the table with backticks, so the same query typed in the
/// editor without them is not taken for a table browse.
pub fn host_browse(statement: &str) -> Option<(String, Range<usize>)> {
    const WRAP_PREFIX: &str = "SELECT * FROM (";
    if starts_with_ignore_case(statement, WRAP_PREFIX) && statement.ends_with(')') {
        let inner = WRAP_PREFIX.len()..statement.len() - 1;
        return browse_table(&statement[inner.clone()]).map(|table| (table, inner));
    }
    browse_table(statement).map(|table| (table, 0..statement.len()))
}

/// ``SELECT * FROM `t` [WHERE …] [ORDER BY …] [LIMIT …]`` → `t`.
fn browse_table(sql: &str) -> Option<String> {
    let rest = strip_prefix_ignore_case(sql, "SELECT * FROM ")?;
    if !rest.starts_with('`') {
        return None;
    }
    let (table, clauses) = parse_identifier(rest)?;
    if (!clauses.is_empty() && !clauses.starts_with(char::is_whitespace)) || !is_balanced(clauses) {
        return None;
    }
    match split_statements(clauses).as_slice() {
        [] => Some(table),
        [clauses]
            if matches!(clauses.first_keyword(), Some("WHERE" | "ORDER" | "LIMIT"))
                && !NOT_RECORDS_KEYWORDS
                    .iter()
                    .any(|k| clauses.has_top_level_keyword(k)) =>
        {
            Some(table)
        }
        _ => None,
    }
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

/// The table of `text` when it is a record id written the way the plugin
/// shows them (`utils::values::format_record_id`): a plain or
/// backtick-quoted table, `:`, and a key that is plain (`bob`, `42`),
/// quoted (`` `weird-id` ``, `⟨weird-id⟩`), a uuid (`u'…'`), or an array or
/// object of plain values (`[2024, 'q1']`). Anything else is not taken for a
/// record id: SurrealDB 3.x casts `'person:a b'` to `person:a`, and 2.x
/// fails the whole query on it.
pub fn record_id_table(text: &str) -> Option<String> {
    let (table, rest) = parse_identifier(text)?;
    let key = rest.strip_prefix(':')?;
    is_record_key(key).then_some(table)
}

fn is_record_key(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some('`'), _) => is_quoted_run(key, '`'),
        (Some('⟨'), _) => is_quoted_run(key, '⟩'),
        (Some('u'), Some(q @ ('\'' | '"'))) => {
            is_quoted_run(&key[1..], q)
                && key[2..key.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '-')
        }
        (Some('[' | '{'), _) => is_plain_collection(key),
        (Some(_), _) => key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        (None, _) => false,
    }
}

/// True when `s` is one non-empty quoted run: its first character opens it
/// and `close` ends it at the very end (`\` escapes).
fn is_quoted_run(s: &str, close: char) -> bool {
    let Some(open) = s.chars().next() else {
        return false;
    };
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == close {
            return i > open.len_utf8() && i + c.len_utf8() == s.len();
        }
    }
    false
}

/// True when `s` is an array or object whose first bracket closes at its
/// end and which holds plain values only: numbers, strings (also `d'…'`,
/// `u'…'`), `true`, `false`, `null`, `none`, and object keys.
fn is_plain_collection(s: &str) -> bool {
    let mut scanner = Scanner::new(s);
    let mut tokens = Vec::new();
    while scanner.next_event().is_some() {
        tokens.push(scanner.token_start..scanner.pos);
        if scanner.depth == 0 && scanner.pos < s.len() {
            return false;
        }
    }
    if scanner.depth != 0 || scanner.unterminated {
        return false;
    }
    tokens.iter().enumerate().all(|(i, token)| {
        let text = &s[token.clone()];
        let next = tokens.get(i + 1);
        match text.chars().next() {
            Some(c) if c.is_ascii_digit() => true,
            Some(c) if c.is_alphanumeric() || c == '_' => {
                ["true", "false", "null", "none"]
                    .iter()
                    .any(|w| text.eq_ignore_ascii_case(w))
                    // An object key.
                    || next.is_some_and(|n| s[n.clone()].starts_with(':'))
                    // A string prefix such as `d'…'`.
                    || next.is_some_and(|n| {
                        n.start == token.end && s[n.clone()].starts_with(['\'', '"'])
                    })
            }
            Some(c) => "'\"[]{},:-+.".contains(c),
            None => false,
        }
    })
}

/// A condition comparing a column with a single-quoted string: the column,
/// the string's value and the byte range of the literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextComparison {
    pub column: String,
    pub value: String,
    pub literal: Range<usize>,
}

/// Statements that can sit inside a filter and have a WHERE of their own.
const NESTED_STATEMENT_KEYWORDS: &[&str] = &[
    "SELECT", "CREATE", "UPDATE", "UPSERT", "DELETE", "INSERT", "RELATE", "RETURN", "LET",
];

/// The `column = '…'` conditions in a filter or a SELECT, the way the host
/// writes a filter by value: a plain or backtick-quoted column, `=`, and a
/// single-quoted string (`''` or `\'` escapes), standing on its own between
/// `WHERE`, `AND`, `OR`, `&&`, `||`, round brackets, the start or the end
/// (or the SELECT's ORDER BY or LIMIT). Field paths (`a.b`), parameters,
/// parts of larger expressions and anything inside `[…]` or `{…}` are not
/// conditions of their own. A statement nested in `sql` could compare the
/// columns of another table, so then there are none.
pub fn text_comparisons(sql: &str) -> Vec<TextComparison> {
    // Each token with whether it sits inside `[…]` or `{…}`.
    let mut tokens: Vec<(Range<usize>, bool)> = Vec::new();
    let mut open: Vec<char> = Vec::new();
    let mut scanner = Scanner::new(sql);
    while let Some(event) = scanner.next_event() {
        let range = scanner.token_start..scanner.pos;
        let text = &sql[range.clone()];
        if matches!(event, Event::Word(_))
            && scanner.depth > 0
            && !scanner.last_word_is_member()
            && NESTED_STATEMENT_KEYWORDS
                .iter()
                .any(|k| text.eq_ignore_ascii_case(k))
        {
            return Vec::new();
        }
        tokens.push((range, open.iter().any(|c| *c != '(')));
        match text {
            "(" | "[" | "{" => open.extend(text.chars()),
            ")" | "]" | "}" => {
                open.pop();
            }
            _ => {}
        }
    }
    let text = |i: usize| tokens.get(i).map(|(r, _)| &sql[r.clone()]);
    let is_any = |t: &str, words: &[&str]| words.iter().any(|w| t.eq_ignore_ascii_case(w));
    let mut out = Vec::new();
    for (i, (column_token, in_collection)) in tokens.iter().enumerate() {
        let starts_condition = match i.checked_sub(1).and_then(text) {
            None => true,
            Some(t) => matches!(t, "(" | "&" | "|") || is_any(t, &["WHERE", "AND", "OR"]),
        };
        if *in_collection || !starts_condition || text(i + 1) != Some("=") {
            continue;
        }
        let Some((column, "")) = parse_identifier(&sql[column_token.clone()]) else {
            continue;
        };
        let Some((literal, _)) = tokens.get(i + 2) else {
            continue;
        };
        let Some((value, rest)) = parse_single_quoted(&sql[literal.start..]) else {
            continue;
        };
        let end = sql.len() - rest.len();
        let after = tokens
            .iter()
            .find(|(r, _)| r.start >= end)
            .map(|(r, _)| &sql[r.clone()]);
        let ends_condition = match after {
            None => true,
            Some(t) => matches!(t, ")" | "&" | "|") || is_any(t, &["AND", "OR", "ORDER", "LIMIT"]),
        };
        if ends_condition {
            out.push(TextComparison {
                column,
                value,
                literal: literal.start..end,
            });
        }
    }
    out
}

/// True when `sql` compares a column with text that is a record id
/// ([`text_comparisons`], [`record_id_table`]).
pub fn compares_text_with_record_ids(sql: &str) -> bool {
    text_comparisons(sql)
        .iter()
        .any(|c| record_id_table(&c.value).is_some())
}

/// Cast the strings that `sql` compares record-link columns with, so the
/// comparison can match: SurrealQL never equals a record with a string. In
/// a [`text_comparisons`] condition `column = 'table:key'`, when `links`
/// says that `column` links to `table` and the text is a record id
/// ([`record_id_table`]), the string becomes `<record> 'table:key'`. The
/// value stays a string literal. `None` when nothing was cast.
pub fn cast_record_link_literals(sql: &str, links: &[(String, Vec<String>)]) -> Option<String> {
    let mut out = String::with_capacity(sql.len());
    let mut copied = 0;
    for comparison in text_comparisons(sql) {
        let Some(table) = record_id_table(&comparison.value) else {
            continue;
        };
        let links_to_table = links
            .iter()
            .any(|(column, tables)| *column == comparison.column && tables.contains(&table));
        if !links_to_table {
            continue;
        }
        out.push_str(&sql[copied..comparison.literal.start]);
        out.push_str("<record> ");
        out.push_str(&crate::utils::values::quote_string(&comparison.value));
        copied = comparison.literal.end;
    }
    (copied > 0).then(|| {
        out.push_str(&sql[copied..]);
        out
    })
}

/// True when `sql` has the shape of one expression: brackets, quotes and
/// regex literals balance, and there is no top-level `;`. It can then be
/// embedded as `WHERE (<sql>\n)` without changing the shape of the
/// statement around it (used for the host's table filter).
///
/// A shape check, not a security guard: one expression can still write
/// (`(DELETE t) = []`, `{ DELETE t; true }`), as the same filter can in the
/// grid, which runs it as typed.
pub fn is_single_expression(sql: &str) -> bool {
    is_balanced(sql) && split_statements(sql).len() <= 1
}

/// True when brackets in `sql` (outside strings, comments and regex
/// literals) balance and never close below zero.
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

/// Keywords after which an expression starts, so a `/` there opens a regex
/// literal.
const KEYWORDS_BEFORE_EXPRESSION: &[&str] = &[
    "AND", "OR", "NOT", "IS", "WHERE", "RETURN", "IF", "THEN", "ELSE", "VALUE",
];

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
    /// The most recent word, to tell a regex literal after a keyword
    /// (`WHERE /x/ …`) from a division after an operand (`n / 2`).
    last_word: Range<usize>,
    /// Where the token that `next_event` returned last starts.
    token_start: usize,
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
            last_word: 0..0,
            token_start: 0,
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
            self.token_start = start;
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
                '/' if self.regex_can_start() => {
                    self.skip_regex();
                    // An operand: a `/` right after it is a division.
                    self.mark_significant('/');
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
                    self.last_word = start..self.pos;
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

    /// True when a `/` here starts a regex literal rather than a division:
    /// where an operand is expected, i.e. at the start, after an operator,
    /// an opening bracket or a separator, or after a keyword that takes an
    /// expression. After an operand (a word, number, string or closing
    /// bracket) it divides.
    fn regex_can_start(&self) -> bool {
        match self.prev_significant {
            None => true,
            Some('a') => {
                let word = &self.src[self.last_word.clone()];
                KEYWORDS_BEFORE_EXPRESSION
                    .iter()
                    .any(|k| word.eq_ignore_ascii_case(k))
            }
            Some(c) => "=!<>~([{,;:?|&+-*%^@".contains(c),
        }
    }

    /// Skip a regex literal `/…/` (with `\/` escapes). A regex is one line;
    /// one without its closing `/` there marks the source unterminated.
    fn skip_regex(&mut self) {
        self.pos += 1;
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.pos += c.len_utf8();
            if c == '\\' {
                if let Some(next) = self.peek().filter(|n| *n != '\n') {
                    self.pos += next.len_utf8();
                }
            } else if c == '/' {
                return;
            }
        }
        self.unterminated = true;
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
    fn finds_statements_that_write() {
        for sql in [
            "SELECT * FROM (UPDATE person SET n += 1)",
            "SELECT * FROM UPDATE person SET n += 1",
            "select * from (delete person:x)",
            "SELECT * FROM person WHERE (CREATE scratch) != []",
            "SELECT * FROM person WHERE id IN [RELATE person:a->knows->person:b]",
            "SELECT * FROM (INSERT INTO t { n: 1 })",
            "SELECT * FROM (UPSERT t:1 SET n = 1)",
            "SELECT * FROM { DEFINE TABLE t; RETURN [] }",
            "SELECT * FROM (REMOVE TABLE t)",
            "SELECT * FROM (ALTER TABLE t COMMENT 'x')",
            "SELECT * FROM (REBUILD INDEX i ON t)",
            "SELECT * FROM (SELECT * FROM (UPDATE t SET n += 1) LIMIT 5)",
        ] {
            assert!(contains_write(sql), "{sql}");
        }
        for sql in [
            "SELECT * FROM person WHERE status = 'update'",
            "SELECT * FROM `update`, ⟨delete⟩ -- update",
            "SELECT a.update, $delete FROM person:create",
            "SELECT array::insert(tags, 'x', 0) FROM person",
            "SELECT * FROM t WHERE { update: 1, delete : 2 }.update = 1",
            "SELECT * FROM person WHERE name = /update/",
            "SELECT ->knows->person AS friends FROM person",
        ] {
            assert!(!contains_write(sql), "{sql}");
        }
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
            // Not a record id at all: 3.x would cast it to `person:a`.
            "SELECT * FROM `person` WHERE `id` = 'person:a b'",
            "SELECT * FROM `person` WHERE `id` = 'person:'",
        ] {
            assert_eq!(normalize_host_browse_query(q), q);
        }
    }

    #[test]
    fn recognises_the_host_table_browse() {
        let table = |sql: &str| host_browse(sql).map(|(t, _)| t);
        for sql in [
            "SELECT * FROM `person`",
            "SELECT * FROM `person` WHERE name = 'Bob' ORDER BY age DESC",
            "SELECT * FROM `person` ORDER BY name",
            "SELECT * FROM `person` WHERE (name = 'x') AND `age` > 3 LIMIT 10",
            // The foreign-key lookup after its rewrite.
            "SELECT * FROM `person` WHERE id = <record> 'person:bob'",
        ] {
            assert_eq!(table(sql).as_deref(), Some("person"), "{sql}");
        }
        assert_eq!(
            table("SELECT * FROM `odd\\`table` WHERE n = 1").as_deref(),
            Some("odd`table")
        );
        // The LIMIT wrapper: the inner SELECT reads the table.
        let sql = "SELECT * FROM (SELECT * FROM `person` WHERE age > 1 LIMIT 50)";
        let (t, range) = host_browse(sql).unwrap();
        assert_eq!(t, "person");
        assert_eq!(&sql[range], "SELECT * FROM `person` WHERE age > 1 LIMIT 50");
        for sql in [
            // Typed in the editor: the host always quotes the table.
            "SELECT * FROM person WHERE name = 'Bob'",
            "SELECT name FROM `person`",
            "SELECT * FROM `person`, `event`",
            "SELECT * FROM `person`:bob",
            "SELECT * FROM `person` WITH NOINDEX WHERE name = 'x'",
            "SELECT * FROM `person` GROUP BY name",
            "SELECT * FROM `person` SPLIT tags",
            "SELECT * FROM `person` WHERE true EXPLAIN",
            "SELECT * FROM `item` WHERE n >= $m }",
            "SELECT * FROM (SELECT 1)",
            "RETURN 1",
        ] {
            assert_eq!(table(sql), None, "{sql}");
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
        // A shape check only: one expression may still write.
        assert!(is_single_expression("(DELETE item) = []"));
    }

    #[test]
    fn regex_literals_are_opaque() {
        // A quote or `;` inside a regex is not code.
        assert!(is_single_expression("name = /it's/"));
        assert!(is_single_expression("string::matches(name, /a;b\\/c/)"));
        assert!(is_single_expression("WHERE /it's/ = name"));
        assert_eq!(
            texts("SELECT * FROM t WHERE s = /x;y/; RETURN 1"),
            vec!["SELECT * FROM t WHERE s = /x;y/", "RETURN 1"]
        );
        // A regex cannot hide a second statement from the check.
        assert!(!is_single_expression(
            "x = /'/) ; DELETE item ; SELECT * FROM item WHERE (x = /'/"
        ));
        assert!(!is_single_expression("x = /unterminated"));
        // After an operand, `/` divides.
        assert_eq!(
            texts("RETURN (10) / 2 / 5; RETURN n / 2; RETURN 'a' / 1"),
            vec!["RETURN (10) / 2 / 5", "RETURN n / 2", "RETURN 'a' / 1"]
        );
        assert!(is_single_expression("count / 2 > 1 AND n / 3 = 1"));
        // Comments still win.
        assert_eq!(
            texts("RETURN 1 // it's; a comment"),
            vec!["RETURN 1 // it's; a comment"]
        );
    }

    #[test]
    fn reads_record_ids_written_like_the_plugin_writes_them() {
        for (text, table) in [
            ("person:bob", "person"),
            ("person:42", "person"),
            ("person:01abc", "person"),
            ("event:`weird-id`", "event"),
            ("event:⟨weird-id⟩", "event"),
            ("event:[2024, 'q1']", "event"),
            ("event:{ region: 'eu', seq: 7 }", "event"),
            (
                "event:[d'2024-01-02T00:00:00Z', -1.5f, true, NONE]",
                "event",
            ),
            ("event:u'0190d6c0-0000-7000-8000-000000000001'", "event"),
            ("`odd table`:x", "odd table"),
        ] {
            assert_eq!(record_id_table(text).as_deref(), Some(table), "{text}");
        }
        // SurrealDB 3.x would cast most of these to another record (the
        // longest prefix it can read) and 2.x would fail the query.
        for text in [
            "Bob",
            "person:",
            ":bob",
            "person bob:1",
            "person:a b",
            "person:bob)",
            "person:bob:2",
            "person:1..5",
            "person:1.5",
            "person:[1, 2]x",
            "person:[1",
            "person:[2024, oops]",
            "person:{ a: oops }",
            "person:`open",
            "person:``",
            "person:u'not-a-uuid'",
        ] {
            assert_eq!(record_id_table(text), None, "{text}");
        }
    }

    #[test]
    fn finds_conditions_comparing_a_column_with_text() {
        let found = |sql: &str| -> Vec<(String, String)> {
            text_comparisons(sql)
                .into_iter()
                .map(|c| {
                    assert!(sql[c.literal.clone()].starts_with('\''), "{sql}");
                    (c.column, c.value)
                })
                .collect()
        };
        let pair = |c: &str, v: &str| (c.to_string(), v.to_string());
        // The popover's filter, alone or added to the tab's own filter.
        assert_eq!(
            found("`best_friend` = 'person:bob'"),
            [pair("best_friend", "person:bob")]
        );
        assert_eq!(
            found("(name = 'Alice') AND `best_friend` = 'person:bob'"),
            [pair("name", "Alice"), pair("best_friend", "person:bob")]
        );
        assert_eq!(
            found("SELECT * FROM `event` WHERE `id` = 'event:[2024, ''q1'']' ORDER BY kind"),
            [pair("id", "event:[2024, 'q1']")]
        );
        assert_eq!(
            found("a = 'x' || (b = 'y' && c = 'z')"),
            [pair("a", "x"), pair("b", "y"), pair("c", "z")]
        );
        // Not a condition of its own, or not `=` with a single-quoted string.
        for sql in [
            "a.best_friend = 'person:bob'",
            "$best_friend = 'person:bob'",
            "x + best_friend = 'person:bob'",
            "best_friend = 'person:bob' + 'x'",
            "best_friend = 'person:bob' IN [true]",
            "best_friend != 'person:bob'",
            "best_friend == 'person:bob'",
            "best_friend = r'person:bob'",
            "best_friend = \"person:bob\"",
            "[best_friend = 'person:bob']",
            // A subquery's WHERE belongs to its own table.
            "best_friend IN (SELECT VALUE id FROM person WHERE best_friend = 'person:bob') OR best_friend = 'person:bob'",
        ] {
            assert_eq!(found(sql), [], "{sql}");
        }
    }

    #[test]
    fn casts_text_compared_with_record_links() {
        let links = vec![
            ("id".to_string(), vec!["person".to_string()]),
            ("best_friend".to_string(), vec!["person".to_string()]),
            (
                "owner".to_string(),
                vec!["person".to_string(), "bot".to_string()],
            ),
        ];
        let cast = |sql: &str| cast_record_link_literals(sql, &links);
        assert_eq!(
            cast("`best_friend` = 'person:bob'").as_deref(),
            Some("`best_friend` = <record> 'person:bob'")
        );
        assert_eq!(
            cast("SELECT * FROM `person` WHERE (name = 'Alice') AND `best_friend` = 'person:bob' AND `id` = 'person:alice' ORDER BY name").as_deref(),
            Some("SELECT * FROM `person` WHERE (name = 'Alice') AND `best_friend` = <record> 'person:bob' AND `id` = <record> 'person:alice' ORDER BY name")
        );
        assert_eq!(
            cast("owner = 'bot:`r2-d2`'").as_deref(),
            Some("owner = <record> 'bot:`r2-d2`'")
        );
        assert_eq!(
            cast("`best_friend` = 'person:[2024, ''q1'']'").as_deref(),
            Some("`best_friend` = <record> 'person:[2024, \\'q1\\']'")
        );
        assert!(compares_text_with_record_ids(
            "`best_friend` = 'person:bob'"
        ));
        // Not a record link, a record of another table, not a record id, or
        // not a condition of its own: untouched.
        for sql in [
            "name = 'person:bob'",
            "best_friend = 'event:1'",
            "best_friend = 'Bob'",
            "best_friend = 'person:a b'",
            "best_friend = 'person:bob' + ''",
        ] {
            assert_eq!(cast(sql), None, "{sql}");
        }
        assert!(!compares_text_with_record_ids(
            "name = 'Bob' AND n = 'person:a b'"
        ));
    }
}
