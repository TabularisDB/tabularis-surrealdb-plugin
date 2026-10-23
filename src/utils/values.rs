//! Conversion from SurrealDB CBOR values to the JSON values Tabularis shows
//! in its grid and JSON viewer.
//!
//! The RPC connection uses CBOR, so every SurrealDB type arrives tagged and
//! nothing is guessed. The host can only display JSON, so each type is
//! mapped as follows (see docs/DESIGN.md, "Value conversion"):
//!
//! | SurrealDB            | Grid cell                                    |
//! |----------------------|----------------------------------------------|
//! | record id            | `table:id` string in SurrealQL syntax         |
//! | object / array       | JSON object / array (converted recursively)   |
//! | int within ±(2^53-1) | JSON number                                   |
//! | int outside that     | decimal string (JavaScript would round it)    |
//! | float                | JSON number; NaN/±Infinity as strings         |
//! | decimal              | exact decimal string                          |
//! | datetime             | RFC 3339 string, UTC, nanosecond precision    |
//! | duration             | SurrealQL duration string (`1h30m`)           |
//! | uuid                 | hyphenated string                             |
//! | bytes                | `0x…` hex string                              |
//! | geometry             | GeoJSON object                                |
//! | NONE and NULL        | JSON `null` (the host has a single null)      |
//!
//! Inside objects, NONE fields are omitted by the server and explicit NULLs
//! stay `null`, so the JSON viewer keeps that distinction.

use ciborium::value::Value as Cbor;
use serde_json::{Map, Number, Value as Json};

use crate::utils::identifiers::{format_identifier, is_plain_identifier, quote_identifier};

/// Largest integer JavaScript represents exactly (`Number.MAX_SAFE_INTEGER`).
pub const JS_MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

pub mod tags {
    pub const DATETIME_STRING: u64 = 0;
    pub const NONE: u64 = 6;
    pub const TABLE: u64 = 7;
    pub const RECORD_ID: u64 = 8;
    pub const UUID_STRING: u64 = 9;
    pub const DECIMAL: u64 = 10;
    pub const DATETIME: u64 = 12;
    pub const DURATION_STRING: u64 = 13;
    pub const DURATION: u64 = 14;
    pub const FUTURE: u64 = 15;
    pub const UUID: u64 = 37;
    pub const RANGE: u64 = 49;
    pub const BOUND_INCLUDED: u64 = 50;
    pub const BOUND_EXCLUDED: u64 = 51;
    pub const GEOMETRY_POINT: u64 = 88;
    pub const GEOMETRY_LINE: u64 = 89;
    pub const GEOMETRY_POLYGON: u64 = 90;
    pub const GEOMETRY_MULTIPOINT: u64 = 91;
    pub const GEOMETRY_MULTILINE: u64 = 92;
    pub const GEOMETRY_MULTIPOLYGON: u64 = 93;
    pub const GEOMETRY_COLLECTION: u64 = 94;
}

/// True for SurrealDB's NONE (CBOR tag 6).
pub fn is_none(value: &Cbor) -> bool {
    matches!(value, Cbor::Tag(tags::NONE, _))
}

/// Convert a value for display in the grid or JSON viewer.
pub fn to_cell(value: &Cbor) -> Json {
    match value {
        Cbor::Null => Json::Null,
        Cbor::Bool(b) => Json::Bool(*b),
        Cbor::Integer(i) => integer_to_json(i128::from(*i)),
        Cbor::Float(f) => float_to_json(*f),
        Cbor::Text(s) => Json::String(s.clone()),
        Cbor::Bytes(b) => Json::String(hex_bytes(b)),
        Cbor::Array(items) => Json::Array(items.iter().map(to_cell).collect()),
        Cbor::Map(entries) => Json::Object(map_to_json(entries)),
        Cbor::Tag(tag, inner) => tagged_to_json(*tag, inner),
        _ => Json::Null,
    }
}

fn map_to_json(entries: &[(Cbor, Cbor)]) -> Map<String, Json> {
    let mut map = Map::with_capacity(entries.len());
    for (key, value) in entries {
        if is_none(value) {
            // NONE inside an object means "field absent".
            continue;
        }
        map.insert(key_to_string(key), to_cell(value));
    }
    map
}

/// Object keys are text in practice; anything else is rendered as SurrealQL.
pub fn key_to_string(key: &Cbor) -> String {
    match key {
        Cbor::Text(s) => s.clone(),
        other => surql_literal(other),
    }
}

fn integer_to_json(i: i128) -> Json {
    if (-JS_MAX_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&i) {
        // In range for i64 by construction.
        Json::Number(Number::from(i as i64))
    } else {
        Json::String(i.to_string())
    }
}

fn float_to_json(f: f64) -> Json {
    match Number::from_f64(f) {
        Some(n) => Json::Number(n),
        None => Json::String(non_finite_name(f).to_string()),
    }
}

fn non_finite_name(f: f64) -> &'static str {
    if f.is_nan() {
        "NaN"
    } else if f.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    }
}

fn tagged_to_json(tag: u64, inner: &Cbor) -> Json {
    match tag {
        tags::NONE => Json::Null,
        tags::RECORD_ID => Json::String(format_record_id_value(inner)),
        tags::TABLE | tags::DECIMAL | tags::UUID_STRING | tags::DATETIME_STRING => match inner {
            Cbor::Text(s) => Json::String(s.clone()),
            other => to_cell(other),
        },
        tags::DATETIME => Json::String(format_datetime_value(inner)),
        tags::DURATION => Json::String(format_duration_value(inner)),
        tags::DURATION_STRING => match inner {
            Cbor::Text(s) => Json::String(s.clone()),
            other => to_cell(other),
        },
        tags::UUID => Json::String(format_uuid_value(inner)),
        tags::FUTURE => Json::String(format!("<future> {}", text_or_literal(inner))),
        tags::RANGE => Json::String(format_range(inner)),
        tags::GEOMETRY_POINT..=tags::GEOMETRY_COLLECTION => geometry_to_geojson(tag, inner),
        // Unknown tag: keep the payload rather than dropping the value.
        _ => to_cell(inner),
    }
}

fn text_or_literal(value: &Cbor) -> String {
    match value {
        Cbor::Text(s) => s.clone(),
        other => surql_literal(other),
    }
}

// ---------------------------------------------------------------------------
// Record ids
// ---------------------------------------------------------------------------

/// Format the payload of a record-id tag (`[table, id]`, or a string the
/// server already formatted) as `table:id` in SurrealQL syntax, so the text
/// can be pasted back into a query.
pub fn format_record_id_value(inner: &Cbor) -> String {
    match inner {
        Cbor::Text(s) => s.clone(),
        Cbor::Array(parts) if parts.len() == 2 => {
            let table = match &parts[0] {
                Cbor::Text(t) => t.clone(),
                other => text_or_literal(other),
            };
            format_record_id(&table, &parts[1])
        }
        other => surql_literal(other),
    }
}

/// `table:id`, quoting the table and the id key where SurrealQL requires it.
pub fn format_record_id(table: &str, id: &Cbor) -> String {
    format!("{}:{}", format_identifier(table), format_id_key(id))
}

fn format_id_key(id: &Cbor) -> String {
    match id {
        Cbor::Integer(i) => {
            let n = i128::from(*i);
            if n < 0 {
                // A leading minus is not part of an identifier; quote it.
                quote_identifier(&n.to_string())
            } else {
                n.to_string()
            }
        }
        Cbor::Text(s) => {
            if is_plain_identifier(s) {
                s.clone()
            } else {
                quote_identifier(s)
            }
        }
        Cbor::Tag(tags::RANGE, inner) => format_range(inner),
        other => surql_literal(other),
    }
}

// ---------------------------------------------------------------------------
// SurrealQL literals (used for complex record-id keys and range bounds)
// ---------------------------------------------------------------------------

/// Render a value as a SurrealQL literal.
pub fn surql_literal(value: &Cbor) -> String {
    match value {
        Cbor::Null => "NULL".to_string(),
        Cbor::Bool(b) => b.to_string(),
        Cbor::Integer(i) => i128::from(*i).to_string(),
        Cbor::Float(f) => {
            if f.is_finite() {
                format!("{f}f")
            } else {
                non_finite_name(*f).to_string()
            }
        }
        Cbor::Text(s) => quote_string(s),
        Cbor::Bytes(b) => format!("<bytes> {}", quote_string(&hex_bytes(b))),
        Cbor::Array(items) => {
            let parts: Vec<String> = items.iter().map(surql_literal).collect();
            format!("[{}]", parts.join(", "))
        }
        Cbor::Map(entries) => {
            if entries.is_empty() {
                return "{}".to_string();
            }
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    let key = key_to_string(k);
                    let key = if is_plain_identifier(&key) {
                        key
                    } else {
                        quote_string(&key)
                    };
                    format!("{key}: {}", surql_literal(v))
                })
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        Cbor::Tag(tag, inner) => match *tag {
            tags::NONE => "NONE".to_string(),
            tags::RECORD_ID => format_record_id_value(inner),
            tags::TABLE => text_or_literal(inner),
            tags::DECIMAL => format!("{}dec", text_or_literal(inner)),
            tags::UUID => format!("u{}", quote_string(&format_uuid_value(inner))),
            tags::UUID_STRING => format!("u{}", quote_string(&text_or_literal(inner))),
            tags::DATETIME => format!("d{}", quote_string(&format_datetime_value(inner))),
            tags::DATETIME_STRING => format!("d{}", quote_string(&text_or_literal(inner))),
            tags::DURATION => format_duration_value(inner),
            tags::DURATION_STRING => text_or_literal(inner),
            tags::RANGE => format_range(inner),
            tags::FUTURE => format!("<future> {}", text_or_literal(inner)),
            _ => surql_literal(inner),
        },
        _ => "NONE".to_string(),
    }
}

/// Single-quoted SurrealQL string with `\` and `'` escaped.
pub fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Range payload `[begin, end]`, each bound tagged included/excluded or null
/// for unbounded. Rendered as `a..b`, `a>..=b`, `..b`, `a..`.
fn format_range(inner: &Cbor) -> String {
    let Cbor::Array(bounds) = inner else {
        return surql_literal(inner);
    };
    let bound = |v: Option<&Cbor>| -> (Option<String>, bool) {
        match v {
            Some(Cbor::Tag(tags::BOUND_INCLUDED, b)) => (Some(range_bound_literal(b)), true),
            Some(Cbor::Tag(tags::BOUND_EXCLUDED, b)) => (Some(range_bound_literal(b)), false),
            Some(Cbor::Null) | None => (None, true),
            Some(other) => (Some(range_bound_literal(other)), true),
        }
    };
    let (begin, begin_included) = bound(bounds.first());
    let (end, end_included) = bound(bounds.get(1));
    let mut out = String::new();
    if let Some(b) = begin {
        out.push_str(&b);
        if !begin_included {
            out.push('>');
        }
    }
    out.push_str("..");
    if let Some(e) = end {
        if end_included {
            out.push('=');
        }
        out.push_str(&e);
    }
    out
}

fn range_bound_literal(value: &Cbor) -> String {
    match value {
        // Record-id ranges carry plain id keys (`person:1..5`).
        Cbor::Text(s) if is_plain_identifier(s) => s.clone(),
        other => surql_literal(other),
    }
}

// ---------------------------------------------------------------------------
// Datetimes, durations, uuids, bytes
// ---------------------------------------------------------------------------

fn as_i128(v: Option<&Cbor>) -> i128 {
    match v {
        Some(Cbor::Integer(i)) => i128::from(*i),
        Some(Cbor::Float(f)) if f.is_finite() => *f as i128,
        _ => 0,
    }
}

/// Compact datetime `[seconds, nanoseconds]` as RFC 3339 UTC.
fn format_datetime_value(inner: &Cbor) -> String {
    match inner {
        Cbor::Array(parts) => {
            let secs = as_i128(parts.first());
            let nanos = as_i128(parts.get(1));
            format_datetime(secs, nanos)
        }
        Cbor::Text(s) => s.clone(),
        other => surql_literal(other),
    }
}

/// Seconds and nanoseconds since the Unix epoch as RFC 3339 UTC, with a
/// fractional part only when it is non-zero (trailing zeros trimmed).
pub fn format_datetime(secs: i128, nanos: i128) -> String {
    let total_nanos = secs * 1_000_000_000 + nanos;
    let secs = total_nanos.div_euclid(1_000_000_000);
    let nanos = total_nanos.rem_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    let mut out = if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}")
    } else {
        format!("{year:+}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}")
    };
    if nanos != 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i128) -> (i128, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Compact duration `[seconds?, nanoseconds?]` in SurrealQL notation.
fn format_duration_value(inner: &Cbor) -> String {
    match inner {
        Cbor::Array(parts) => format_duration(as_i128(parts.first()), as_i128(parts.get(1))),
        Cbor::Text(s) => s.clone(),
        other => surql_literal(other),
    }
}

/// SurrealQL duration text, largest unit first (`1y2w3d4h5m6s7ms8µs9ns`).
/// A year is 365 days and a week 7 days, as in SurrealDB.
pub fn format_duration(secs: i128, nanos: i128) -> String {
    let total = secs * 1_000_000_000 + nanos;
    if total == 0 {
        return "0ns".to_string();
    }
    const NS: i128 = 1;
    const US: i128 = 1_000 * NS;
    const MS: i128 = 1_000 * US;
    const S: i128 = 1_000 * MS;
    const M: i128 = 60 * S;
    const H: i128 = 60 * M;
    const D: i128 = 24 * H;
    const W: i128 = 7 * D;
    const Y: i128 = 365 * D;
    let units: [(i128, &str); 9] = [
        (Y, "y"),
        (W, "w"),
        (D, "d"),
        (H, "h"),
        (M, "m"),
        (S, "s"),
        (MS, "ms"),
        (US, "µs"),
        (NS, "ns"),
    ];
    let mut rest = total.abs();
    let mut out = String::new();
    if total < 0 {
        out.push('-');
    }
    for (size, suffix) in units {
        let n = rest / size;
        if n > 0 {
            out.push_str(&n.to_string());
            out.push_str(suffix);
            rest -= n * size;
        }
    }
    out
}

fn format_uuid_value(inner: &Cbor) -> String {
    match inner {
        Cbor::Bytes(b) if b.len() == 16 => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            )
        }
        Cbor::Text(s) => s.clone(),
        other => surql_literal(other),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

fn geometry_to_geojson(tag: u64, inner: &Cbor) -> Json {
    let kind = match tag {
        tags::GEOMETRY_POINT => "Point",
        tags::GEOMETRY_LINE => "LineString",
        tags::GEOMETRY_POLYGON => "Polygon",
        tags::GEOMETRY_MULTIPOINT => "MultiPoint",
        tags::GEOMETRY_MULTILINE => "MultiLineString",
        tags::GEOMETRY_MULTIPOLYGON => "MultiPolygon",
        _ => "GeometryCollection",
    };
    let mut obj = Map::new();
    obj.insert("type".to_string(), Json::String(kind.to_string()));
    if tag == tags::GEOMETRY_COLLECTION {
        let geometries = match inner {
            Cbor::Array(items) => items.iter().map(to_cell).collect(),
            _ => Vec::new(),
        };
        obj.insert("geometries".to_string(), Json::Array(geometries));
    } else {
        obj.insert("coordinates".to_string(), geometry_coordinates(inner));
    }
    Json::Object(obj)
}

/// Strip geometry tags and keep the nested coordinate arrays.
fn geometry_coordinates(value: &Cbor) -> Json {
    match value {
        Cbor::Tag(_, inner) => geometry_coordinates(inner),
        Cbor::Array(items) => Json::Array(items.iter().map(geometry_coordinates).collect()),
        other => to_cell(other),
    }
}

// ---------------------------------------------------------------------------
// Type names (for columns inferred from sampled records)
// ---------------------------------------------------------------------------

/// SurrealQL type name of a value, as used in `DEFINE FIELD … TYPE`.
pub fn kind_name(value: &Cbor) -> &'static str {
    match value {
        Cbor::Null => "null",
        Cbor::Bool(_) => "bool",
        Cbor::Integer(_) => "int",
        Cbor::Float(_) => "float",
        Cbor::Text(_) => "string",
        Cbor::Bytes(_) => "bytes",
        Cbor::Array(_) => "array",
        Cbor::Map(_) => "object",
        Cbor::Tag(tag, _) => match *tag {
            tags::NONE => "none",
            tags::TABLE => "string",
            tags::RECORD_ID => "record",
            tags::UUID | tags::UUID_STRING => "uuid",
            tags::DECIMAL => "decimal",
            tags::DATETIME | tags::DATETIME_STRING => "datetime",
            tags::DURATION | tags::DURATION_STRING => "duration",
            tags::FUTURE => "future",
            tags::RANGE => "range",
            tags::GEOMETRY_POINT..=tags::GEOMETRY_COLLECTION => "geometry",
            _ => "any",
        },
        _ => "any",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tag(t: u64, v: Cbor) -> Cbor {
        Cbor::Tag(t, Box::new(v))
    }
    fn text(s: &str) -> Cbor {
        Cbor::Text(s.to_string())
    }
    fn int(i: i64) -> Cbor {
        Cbor::Integer(i.into())
    }
    fn rid(table: &str, id: Cbor) -> Cbor {
        tag(tags::RECORD_ID, Cbor::Array(vec![text(table), id]))
    }

    #[test]
    fn record_ids_render_as_usable_surrealql() {
        assert_eq!(
            to_cell(&rid("person", text("alice"))),
            json!("person:alice")
        );
        assert_eq!(to_cell(&rid("event", int(1))), json!("event:1"));
        assert_eq!(
            to_cell(&rid("event", text("weird-id"))),
            json!("event:`weird-id`")
        );
        assert_eq!(to_cell(&rid("event", text("123"))), json!("event:`123`"));
        assert_eq!(
            to_cell(&rid("odd table", text("x"))),
            json!("`odd table`:x")
        );
        assert_eq!(
            to_cell(&rid("event", Cbor::Array(vec![int(1), text("a")]))),
            json!("event:[1, 'a']")
        );
        assert_eq!(
            to_cell(&rid(
                "temp",
                Cbor::Map(vec![(text("city"), text("Rome")), (text("at day"), int(2))])
            )),
            json!("temp:{ city: 'Rome', 'at day': 2 }")
        );
        assert_eq!(
            to_cell(&rid("u", tag(tags::UUID, Cbor::Bytes(vec![0x01; 16])))),
            json!("u:u'01010101-0101-0101-0101-010101010101'")
        );
        // A string the server already formatted is kept verbatim.
        assert_eq!(to_cell(&tag(tags::RECORD_ID, text("a:b"))), json!("a:b"));
    }

    #[test]
    fn record_id_ranges() {
        let range = tag(
            tags::RANGE,
            Cbor::Array(vec![
                tag(tags::BOUND_INCLUDED, int(1)),
                tag(tags::BOUND_EXCLUDED, int(5)),
            ]),
        );
        assert_eq!(to_cell(&rid("t", range)), json!("t:1..5"));
        let open = tag(
            tags::RANGE,
            Cbor::Array(vec![Cbor::Null, tag(tags::BOUND_INCLUDED, int(9))]),
        );
        assert_eq!(to_cell(&open), json!("..=9"));
    }

    #[test]
    fn nested_values_stay_json_and_keep_none_vs_null() {
        let v = Cbor::Map(vec![
            (text("id"), rid("person", text("bob"))),
            (
                text("address"),
                Cbor::Map(vec![(text("city"), text("Oslo"))]),
            ),
            (text("tags"), Cbor::Array(vec![text("a"), Cbor::Null])),
            (text("gone"), tag(tags::NONE, Cbor::Null)),
            (text("empty"), Cbor::Null),
            (text("friend"), rid("person", text("alice"))),
        ]);
        assert_eq!(
            to_cell(&v),
            json!({
                "id": "person:bob",
                "address": { "city": "Oslo" },
                "tags": ["a", null],
                "empty": null,
                "friend": "person:alice"
            })
        );
    }

    #[test]
    fn none_and_null_scalars_both_become_null() {
        assert_eq!(to_cell(&tag(tags::NONE, Cbor::Null)), Json::Null);
        assert_eq!(to_cell(&Cbor::Null), Json::Null);
        assert!(is_none(&tag(tags::NONE, Cbor::Null)));
        assert!(!is_none(&Cbor::Null));
    }

    #[test]
    fn numbers_keep_precision() {
        assert_eq!(to_cell(&int(42)), json!(42));
        assert_eq!(
            to_cell(&int(9_007_199_254_740_991)),
            json!(9_007_199_254_740_991_i64)
        );
        assert_eq!(
            to_cell(&int(9_007_199_254_740_992)),
            json!("9007199254740992")
        );
        assert_eq!(to_cell(&int(i64::MAX)), json!("9223372036854775807"));
        assert_eq!(to_cell(&int(i64::MIN)), json!("-9223372036854775808"));
        assert_eq!(
            to_cell(&tag(tags::DECIMAL, text("12.34567890123456789"))),
            json!("12.34567890123456789")
        );
        assert_eq!(to_cell(&Cbor::Float(1.5)), json!(1.5));
        assert_eq!(to_cell(&Cbor::Float(f64::NAN)), json!("NaN"));
        assert_eq!(to_cell(&Cbor::Float(f64::NEG_INFINITY)), json!("-Infinity"));
    }

    #[test]
    fn datetimes() {
        let dt = |s: i64, n: i64| tag(tags::DATETIME, Cbor::Array(vec![int(s), int(n)]));
        assert_eq!(
            to_cell(&dt(1_704_164_645, 0)),
            json!("2024-01-02T03:04:05Z")
        );
        assert_eq!(
            to_cell(&dt(0, 120_000_000)),
            json!("1970-01-01T00:00:00.12Z")
        );
        assert_eq!(to_cell(&dt(-1, 0)), json!("1969-12-31T23:59:59Z"));
        assert_eq!(
            to_cell(&dt(951_782_400, 1)),
            json!("2000-02-29T00:00:00.000000001Z")
        );
        assert_eq!(
            to_cell(&tag(tags::DATETIME_STRING, text("2024-01-02T03:04:05Z"))),
            json!("2024-01-02T03:04:05Z")
        );
    }

    #[test]
    fn durations() {
        let d = |s: i64, n: i64| tag(tags::DURATION, Cbor::Array(vec![int(s), int(n)]));
        assert_eq!(to_cell(&d(5400, 0)), json!("1h30m"));
        assert_eq!(to_cell(&d(0, 1_500)), json!("1µs500ns"));
        assert_eq!(
            to_cell(&tag(tags::DURATION, Cbor::Array(vec![]))),
            json!("0ns")
        );
        assert_eq!(format_duration(365 * 86_400 + 8 * 86_400, 0), "1y1w1d");
    }

    #[test]
    fn uuids_bytes_tables_futures() {
        let bytes: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            to_cell(&tag(tags::UUID, Cbor::Bytes(bytes))),
            json!("00010203-0405-0607-0809-0a0b0c0d0e0f")
        );
        assert_eq!(to_cell(&Cbor::Bytes(vec![0xde, 0xad])), json!("0xdead"));
        assert_eq!(to_cell(&tag(tags::TABLE, text("person"))), json!("person"));
        assert_eq!(
            to_cell(&tag(tags::FUTURE, text("{ time::now() }"))),
            json!("<future> { time::now() }")
        );
    }

    #[test]
    fn geometry_becomes_geojson() {
        let point = |x: f64, y: f64| {
            tag(
                tags::GEOMETRY_POINT,
                Cbor::Array(vec![Cbor::Float(x), Cbor::Float(y)]),
            )
        };
        assert_eq!(
            to_cell(&point(12.5, 41.9)),
            json!({ "type": "Point", "coordinates": [12.5, 41.9] })
        );
        let line = tag(
            tags::GEOMETRY_LINE,
            Cbor::Array(vec![point(0.0, 0.0), point(1.0, 1.0)]),
        );
        assert_eq!(
            to_cell(&line),
            json!({ "type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]] })
        );
        let collection = tag(
            tags::GEOMETRY_COLLECTION,
            Cbor::Array(vec![point(1.0, 2.0)]),
        );
        assert_eq!(
            to_cell(&collection),
            json!({ "type": "GeometryCollection", "geometries": [
                { "type": "Point", "coordinates": [1.0, 2.0] }
            ] })
        );
    }

    #[test]
    fn unknown_tags_keep_their_payload() {
        assert_eq!(to_cell(&tag(999, text("x"))), json!("x"));
    }

    #[test]
    fn kind_names() {
        assert_eq!(kind_name(&rid("a", int(1))), "record");
        assert_eq!(kind_name(&tag(tags::DECIMAL, text("1"))), "decimal");
        assert_eq!(kind_name(&Cbor::Map(vec![])), "object");
        assert_eq!(kind_name(&int(1)), "int");
    }

    #[test]
    fn string_literals_escape_quotes_and_backslashes() {
        assert_eq!(quote_string("it's"), "'it\\'s'");
        assert_eq!(quote_string("a\\b"), "'a\\\\b'");
    }
}
