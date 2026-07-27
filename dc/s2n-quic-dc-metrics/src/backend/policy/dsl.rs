// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A small, dependency-free text grammar for [`Policy`] rules, so a deployment can configure
//! filtering/collapse from a config file (e.g. a TOML array of strings) instead of Rust.
//!
//! # Grammar
//!
//! One string is one rule: an **action**, optionally followed by `where` and a **predicate**.
//!
//! ```text
//! rule       := action ( "where" predicate )?
//! action     := "keep" | "drop" | "collapse" IDENT
//! predicate  := or
//! or         := and ( "or" and )*
//! and        := unary ( "and" unary )*
//! unary      := "not" unary | primary
//! primary    := "(" or ")" | leaf
//! leaf       := "name" op value
//!             | "agg" "." IDENT                 // aggregation-key presence
//!             | IDENT ( op value | "in" "(" value_list ")" )?   // metadata tag; bare = present
//! op         := "=" | "^=" | "~"                // exact | prefix | glob
//! value      := BAREWORD | "*" | "'" ... "'"    // "*" means "any value" (presence)
//! value_list := value ( "," value )*
//! ```
//!
//! `and`/`or`/`not`/`where`/`in` are reserved keywords (case-insensitive); `name` and a leading
//! `agg.` are reserved leaf forms. Quote a value in `'...'` to use a reserved word or a value with
//! spaces/punctuation literally. Whitespace is insignificant.
//!
//! # Examples
//!
//! ```text
//! collapse worker where name ^= 'send.' and level = debug
//! keep where name = 'send.critical'
//! drop where name ~ 'tx.acked.frame.*'
//! collapse worker where level in (debug, info, warn)
//! drop where level                       // any metric carrying a `level` tag
//! collapse worker where not name ~ 'rx.*'
//! ```
//!
//! Rule **priority** is derived from matcher shape (as with the builder API); the DSL has no
//! priority syntax in this version.

use super::{Action, Matcher, Policy};
use std::{fmt, sync::Arc};

/// An error parsing a [`Policy`] rule expression, with the byte offset into the input where it was
/// detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePolicyError {
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Byte offset into the source string where the error was detected.
    pub position: usize,
}

impl fmt::Display for ParsePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "policy parse error at offset {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for ParsePolicyError {}

// ── Lexer ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A bareword or keyword (`send.lost`, `and`, `name`, `collapse`). Kept verbatim; keyword-ness
    /// is decided by the parser, case-insensitively.
    Ident(String),
    /// A single-quoted string literal, with the quotes stripped.
    Str(String),
    LParen,
    RParen,
    Comma,
    /// `=`
    Eq,
    /// `^=`
    PrefixEq,
    /// `~`
    Tilde,
    /// `*` (only meaningful as a value → "any").
    Star,
}

/// A token plus the byte offset it started at (for error reporting).
struct Spanned {
    tok: Tok,
    at: usize,
}

/// Characters allowed unquoted in a bareword: metric names and tag values use dots, colons,
/// slashes, and dashes, so keep them. Delimiters and operators terminate a bareword.
fn is_bareword_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '/')
}

fn lex(input: &str) -> Result<Vec<Spanned>, ParsePolicyError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Spanned {
                    tok: Tok::LParen,
                    at: i,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    tok: Tok::RParen,
                    at: i,
                });
                i += 1;
            }
            ',' => {
                out.push(Spanned {
                    tok: Tok::Comma,
                    at: i,
                });
                i += 1;
            }
            '=' => {
                out.push(Spanned {
                    tok: Tok::Eq,
                    at: i,
                });
                i += 1;
            }
            '~' => {
                out.push(Spanned {
                    tok: Tok::Tilde,
                    at: i,
                });
                i += 1;
            }
            '*' => {
                out.push(Spanned {
                    tok: Tok::Star,
                    at: i,
                });
                i += 1;
            }
            '^' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Spanned {
                        tok: Tok::PrefixEq,
                        at: i,
                    });
                    i += 2;
                } else {
                    return Err(ParsePolicyError {
                        message: "expected `=` after `^`".into(),
                        position: i,
                    });
                }
            }
            '\'' => {
                // Single-quoted literal; no escape sequences (values never need a quote inside).
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'\'' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(ParsePolicyError {
                        message: "unterminated quoted value".into(),
                        position: i,
                    });
                }
                out.push(Spanned {
                    tok: Tok::Str(input[start..j].to_string()),
                    at: i,
                });
                i = j + 1;
            }
            c if is_bareword_char(c) => {
                let start = i;
                while i < bytes.len() && is_bareword_char(input[i..].chars().next().unwrap()) {
                    i += input[i..].chars().next().unwrap().len_utf8();
                }
                out.push(Spanned {
                    tok: Tok::Ident(input[start..i].to_string()),
                    at: start,
                });
            }
            _ => {
                return Err(ParsePolicyError {
                    message: format!("unexpected character `{c}`"),
                    position: i,
                });
            }
        }
    }
    Ok(out)
}

// ── Parser ──────────────────────────────────────────────────────────────────

struct Parser<'a> {
    toks: &'a [Spanned],
    pos: usize,
    /// Offset just past the end of input, for end-of-input errors.
    end: usize,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Spanned], end: usize) -> Self {
        Parser { toks, pos: 0, end }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|s| &s.tok)
    }

    fn at(&self) -> usize {
        self.toks.get(self.pos).map(|s| s.at).unwrap_or(self.end)
    }

    fn bump(&mut self) -> Option<&Spanned> {
        let s = self.toks.get(self.pos);
        if s.is_some() {
            self.pos += 1;
        }
        s
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParsePolicyError> {
        Err(ParsePolicyError {
            message: message.into(),
            position: self.at(),
        })
    }

    /// Consumes the next token if it is a keyword matching `kw` (case-insensitive). Keywords are
    /// only ever `Ident`s.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    // ── rule ──────────────────────────────────────────────────────────────

    /// Parses a full rule: `action ( "where" predicate )?`.
    fn parse_rule(&mut self) -> Result<(Matcher, Action), ParsePolicyError> {
        let action = self.parse_action()?;
        let matcher = if self.eat_keyword("where") {
            self.parse_predicate()?
        } else {
            Matcher::always()
        };
        if self.pos != self.toks.len() {
            return self.err("unexpected trailing input after rule");
        }
        Ok((matcher, action))
    }

    fn parse_action(&mut self) -> Result<Action, ParsePolicyError> {
        // The action keyword is a bareword in the first position.
        let Some(Tok::Ident(word)) = self.peek().cloned() else {
            return self.err("expected an action (`keep`, `drop`, or `collapse <key>`)");
        };
        if word.eq_ignore_ascii_case("keep") {
            self.pos += 1;
            Ok(Action::Keep)
        } else if word.eq_ignore_ascii_case("drop") {
            self.pos += 1;
            Ok(Action::Drop)
        } else if word.eq_ignore_ascii_case("collapse") {
            self.pos += 1;
            let key = self.parse_value_word("an aggregation key to collapse")?;
            Ok(Action::Collapse { key })
        } else {
            self.err(format!(
                "expected an action (`keep`, `drop`, or `collapse <key>`), found `{word}`"
            ))
        }
    }

    // ── predicate ───────────────────────────────────────────────────────────

    fn parse_predicate(&mut self) -> Result<Matcher, ParsePolicyError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Matcher, ParsePolicyError> {
        let mut terms = vec![self.parse_and()?];
        while self.eat_keyword("or") {
            terms.push(self.parse_and()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Matcher::any_of(terms)
        })
    }

    fn parse_and(&mut self) -> Result<Matcher, ParsePolicyError> {
        let mut terms = vec![self.parse_unary()?];
        while self.eat_keyword("and") {
            terms.push(self.parse_unary()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Matcher::all(terms)
        })
    }

    fn parse_unary(&mut self) -> Result<Matcher, ParsePolicyError> {
        if self.eat_keyword("not") {
            Ok(self.parse_unary()?.not())
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Matcher, ParsePolicyError> {
        if matches!(self.peek(), Some(Tok::LParen)) {
            self.bump();
            let inner = self.parse_or()?;
            if !matches!(self.peek(), Some(Tok::RParen)) {
                return self.err("expected `)`");
            }
            self.bump();
            Ok(inner)
        } else {
            self.parse_leaf()
        }
    }

    // ── leaf ──────────────────────────────────────────────────────────────

    fn parse_leaf(&mut self) -> Result<Matcher, ParsePolicyError> {
        let Some(Tok::Ident(word)) = self.peek().cloned() else {
            return self.err("expected a predicate (name/tag/agg constraint)");
        };

        // `name <op> <value>` — the metric name.
        if word.eq_ignore_ascii_case("name") {
            self.pos += 1;
            return self.parse_name_leaf();
        }

        // `agg.<key>` — aggregation-key presence. The lexer keeps `agg.worker` as one bareword.
        if let Some(key) = word.strip_prefix("agg.") {
            if key.is_empty() {
                return self.err("`agg.` must be followed by a key");
            }
            self.pos += 1;
            return Ok(Matcher::agg_key(key));
        }

        // Otherwise a metadata-tag key: `key`, `key <op> value`, or `key in (...)`.
        self.pos += 1;
        self.parse_tag_leaf(word)
    }

    fn parse_name_leaf(&mut self) -> Result<Matcher, ParsePolicyError> {
        match self.peek() {
            Some(Tok::Eq) => {
                self.bump();
                if matches!(self.peek(), Some(Tok::Star)) {
                    self.bump();
                    // `name = *` — any name.
                    return Ok(Matcher::glob("*"));
                }
                let v = self.parse_value_word("a name value")?;
                Ok(Matcher::name(v))
            }
            Some(Tok::PrefixEq) => {
                self.bump();
                let v = self.parse_value_word("a name prefix")?;
                Ok(Matcher::prefix(v))
            }
            Some(Tok::Tilde) => {
                self.bump();
                let v = self.parse_value_word("a name glob")?;
                Ok(Matcher::glob(v))
            }
            Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("in") => {
                self.bump();
                let values = self.parse_value_list()?;
                // `name in (...)` — OR of exact-name matches.
                Ok(Matcher::any_of(values.into_iter().map(Matcher::name)))
            }
            _ => self.err("expected `=`, `^=`, `~`, or `in` after `name`"),
        }
    }

    fn parse_tag_leaf(&mut self, key: String) -> Result<Matcher, ParsePolicyError> {
        match self.peek() {
            Some(Tok::Eq) => {
                self.bump();
                if matches!(self.peek(), Some(Tok::Star)) {
                    self.bump();
                    // `key = *` — present with any value.
                    return Ok(Matcher::tag_present(key));
                }
                let v = self.parse_value_word("a tag value")?;
                Ok(Matcher::tag(key, v))
            }
            Some(Tok::Tilde) => {
                self.bump();
                let v = self.parse_value_word("a tag-value glob")?;
                Ok(Matcher::tag_glob(key, v))
            }
            Some(Tok::PrefixEq) => {
                self.bump();
                // Prefix on a tag value maps to a glob `<prefix>*`.
                let v = self.parse_value_word("a tag-value prefix")?;
                Ok(Matcher::tag_glob(key, format!("{v}*")))
            }
            Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("in") => {
                self.bump();
                let values = self.parse_value_list()?;
                Ok(Matcher::tag_any_of(key, values))
            }
            // Bare `key` — the tag is present with any value.
            _ => Ok(Matcher::tag_present(key)),
        }
    }

    // ── values ──────────────────────────────────────────────────────────────

    /// Parses a single value token (bareword or quoted string) into an owned `Arc<str>`. A reserved
    /// keyword may only be used as a value when quoted.
    fn parse_value_word(&mut self, what: &str) -> Result<Arc<str>, ParsePolicyError> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s: Arc<str> = Arc::from(s.as_str());
                self.bump();
                Ok(s)
            }
            Some(Tok::Ident(s)) => {
                if is_reserved(s) {
                    return self.err(format!(
                        "expected {what}, found reserved keyword `{s}` (quote it as '{s}' to use literally)"
                    ));
                }
                let s: Arc<str> = Arc::from(s.as_str());
                self.bump();
                Ok(s)
            }
            _ => self.err(format!("expected {what}")),
        }
    }

    /// Parses `( value ( , value )* )`.
    fn parse_value_list(&mut self) -> Result<Vec<Arc<str>>, ParsePolicyError> {
        if !matches!(self.peek(), Some(Tok::LParen)) {
            return self.err("expected `(` after `in`");
        }
        self.bump();
        let mut values = Vec::new();
        loop {
            values.push(self.parse_value_word("a value")?);
            match self.peek() {
                Some(Tok::Comma) => {
                    self.bump();
                }
                Some(Tok::RParen) => {
                    self.bump();
                    break;
                }
                _ => return self.err("expected `,` or `)` in value list"),
            }
        }
        if values.is_empty() {
            return self.err("`in (...)` needs at least one value");
        }
        Ok(values)
    }
}

/// Reserved keywords that must be quoted to use as literal values.
fn is_reserved(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "and" | "or" | "not" | "where" | "in"
    )
}

/// Parses a single rule expression into a `(Matcher, Action)` pair.
pub(super) fn parse_rule(input: &str) -> Result<(Matcher, Action), ParsePolicyError> {
    let toks = lex(input)?;
    let mut parser = Parser::new(&toks, input.len());
    if parser.peek().is_none() {
        return Err(ParsePolicyError {
            message: "empty rule".into(),
            position: 0,
        });
    }
    parser.parse_rule()
}

impl std::str::FromStr for Matcher {
    type Err = ParsePolicyError;

    /// Parses a bare **predicate** expression (no action), e.g. `name ^= 'send.' and level = debug`.
    /// To parse a full rule (with an action), use [`Policy::rule_expr`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let toks = lex(s)?;
        let mut parser = Parser::new(&toks, s.len());
        if parser.peek().is_none() {
            return Err(ParsePolicyError {
                message: "empty matcher expression".into(),
                position: 0,
            });
        }
        let matcher = parser.parse_predicate()?;
        if parser.pos != parser.toks.len() {
            return parser.err("unexpected trailing input after predicate");
        }
        Ok(matcher)
    }
}

impl std::str::FromStr for Policy {
    type Err = ParsePolicyError;

    /// Parses a whole policy: one rule per non-empty, non-`#`-comment line.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Policy::from_exprs(s.lines())
    }
}

#[cfg(test)]
mod test {
    use super::super::{Action, Matcher, Policy};
    use crate::{
        backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions},
        Registry, Unit,
    };

    /// A minimal capture backend recording emitted counter `(name, aggregation, value)`.
    #[derive(Default)]
    struct Capture {
        counters: Vec<(String, Option<String>, u64)>,
    }
    impl Backend for Capture {
        fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
            self.counters.push((
                info.name.to_string(),
                info.aggregation.map(|a| a.to_string()),
                value,
            ));
        }
        fn record_gauge(&mut self, _: &MetricInfo<'_>, _: i64) {}
        fn record_bool(&mut self, _: &MetricInfo<'_>, _: u64, _: u64) {}
        fn record_histogram(&mut self, _: &MetricInfo<'_>, _: Histogram<'_>) {}
        fn record_callback(&mut self, _: &MetricInfo<'_>, _: &[&dyn CallbackValue]) {}
    }

    fn names(cap: &Capture) -> Vec<&str> {
        cap.counters.iter().map(|(n, _, _)| n.as_str()).collect()
    }

    /// The exact config-facing example lines must parse (locks the documented grammar contract).
    #[test]
    fn documented_examples_parse() {
        let policy: Policy = Policy::from_exprs([
            "collapse worker where name ^= 'send.' and level = debug",
            "keep where name = 'send.critical'",
            "drop where name ~ 'tx.acked.frame.*'",
            "collapse worker where level in (debug, info, warn)",
            "drop where level",
            "collapse worker where not name ~ 'rx.*'",
        ])
        .unwrap();
        assert_eq!(policy.rule_count(), 6);

        // And the whole-policy `FromStr` (one rule per line, `#` comments skipped) is equivalent.
        let text = "# statsd\ncollapse worker where name ^= 'send.' and level = debug\nkeep where name = 'send.critical'";
        let policy: Policy = text.parse().unwrap();
        assert_eq!(policy.rule_count(), 2);
    }

    #[test]
    fn parses_each_action() {
        assert!(matches!(super::parse_rule("keep").unwrap().1, Action::Keep));
        assert!(matches!(super::parse_rule("drop").unwrap().1, Action::Drop));
        match super::parse_rule("collapse worker").unwrap().1 {
            Action::Collapse { key } => assert_eq!(&*key, "worker"),
            other => panic!("expected collapse, got {other:?}"),
        }
    }

    #[test]
    fn where_clause_drives_matching() {
        // A predicate `FromStr` parses standalone.
        let m: Matcher = "name ^= 'send.' and level = debug".parse().unwrap();
        // Build metrics: a matching one and two non-matching.
        let name: std::sync::Arc<str> = std::sync::Arc::from("send.lost");
        let agg: std::sync::Arc<str> = std::sync::Arc::from("worker|send.0");
        let tags = crate::tags::metric_tag_set([("level", "debug")]);
        let mut info = MetricInfo::new(
            &name,
            Some(&agg),
            Unit::Count,
            crate::backend::MetricKind::Counter,
        );
        info.tags = crate::MetricTags::new(&tags);
        assert!(super::super::Matcher::matches(&m, &info));

        // Wrong tag value.
        let tags2 = crate::tags::metric_tag_set([("level", "info")]);
        info.tags = crate::MetricTags::new(&tags2);
        assert!(!super::super::Matcher::matches(&m, &info));
    }

    /// End-to-end: a DSL policy collapses per-worker `send.*` debug metrics through `Filtered`,
    /// exactly like the builder equivalent.
    #[test]
    fn dsl_policy_collapses_like_builder() {
        let build = || {
            let registry = Registry::new();
            for w in 0..4 {
                registry
                    .metric("send.lost")
                    .aggregation(format!("worker|send.{w}"))
                    .tag("level", "debug")
                    .counter()
                    .increment(1);
            }
            registry
                .register_counter("rx.data".into(), None)
                .increment(9);
            registry
        };

        let policy: Policy = Policy::from_exprs([
            "collapse worker where name ^= 'send.' and level = debug",
            "keep where name = 'rx.data'",
        ])
        .unwrap();

        let registry = build();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        let cap = backend.inner();

        // The 4 worker series collapse to one; rx.data passes through.
        assert!(cap.counters.contains(&("send.lost".to_string(), None, 4)));
        assert!(cap.counters.contains(&("rx.data".to_string(), None, 9)));
        assert_eq!(cap.counters.len(), 2);
    }

    #[test]
    fn drop_and_keep_and_glob_and_in() {
        let registry = Registry::new();
        registry
            .metric("a")
            .tag("level", "debug")
            .counter()
            .increment(1);
        registry
            .metric("b")
            .tag("level", "info")
            .counter()
            .increment(1);
        registry
            .metric("c")
            .tag("level", "warn")
            .counter()
            .increment(1);
        registry
            .register_counter("tx.acked.frame.x".into(), None)
            .increment(1);

        let policy: Policy = Policy::from_exprs([
            "drop where name ~ 'tx.acked.frame.*'",
            "drop where level in (debug, info)",
        ])
        .unwrap();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        // Only `c` (level=warn, not in the dropped set, not the frame prefix) survives.
        assert_eq!(names(backend.inner()), vec!["c"]);
    }

    #[test]
    fn not_and_parens() {
        let registry = Registry::new();
        registry
            .register_counter("send.x".into(), None)
            .increment(1);
        registry
            .register_counter("recv.x".into(), None)
            .increment(1);
        registry.register_counter("other".into(), None).increment(1);

        // Drop everything that is NOT (send.* or recv.*).
        let policy: Policy =
            Policy::from_exprs(["drop where not (name ^= 'send.' or name ^= 'recv.')"]).unwrap();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        let mut got = names(backend.inner());
        got.sort();
        assert_eq!(got, vec!["recv.x", "send.x"]);
    }

    #[test]
    fn bare_tag_key_is_presence() {
        let registry = Registry::new();
        registry
            .metric("a")
            .tag("level", "debug")
            .counter()
            .increment(1);
        registry.register_counter("b".into(), None).increment(1);

        let policy: Policy = Policy::from_exprs(["drop where level"]).unwrap();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        assert_eq!(names(backend.inner()), vec!["b"]);
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let policy: Policy =
            "# statsd policy\n\ncollapse worker where level = debug\n  # trailing\n"
                .parse()
                .unwrap();
        // One rule parsed despite the comment/blank lines.
        assert_eq!(policy.rule_count(), 1);
    }

    #[test]
    fn error_messages_have_positions() {
        // Unknown action.
        let e = super::parse_rule("frobnicate where x").unwrap_err();
        assert_eq!(e.position, 0);
        assert!(e.message.contains("action"), "{}", e.message);

        // Missing operator after name.
        let e = super::parse_rule("keep where name").unwrap_err();
        assert!(e.message.contains("`=`"), "{}", e.message);

        // Reserved keyword used unquoted as a value.
        let e = super::parse_rule("drop where name = and").unwrap_err();
        assert!(e.message.contains("reserved"), "{}", e.message);

        // Unterminated quote.
        let e = super::parse_rule("drop where name = 'oops").unwrap_err();
        assert!(e.message.contains("unterminated"), "{}", e.message);

        // Trailing junk.
        let e = super::parse_rule("keep extra").unwrap_err();
        assert!(e.message.contains("trailing"), "{}", e.message);
    }

    #[test]
    fn agg_key_and_name_in_list() {
        let registry = Registry::new();
        // Two per-worker series (carry a `worker` aggregation key) + one without.
        registry
            .metric("m")
            .aggregation("worker|send.0")
            .counter()
            .increment(1);
        registry
            .metric("m")
            .aggregation("worker|send.1")
            .counter()
            .increment(1);
        registry.register_counter("plain".into(), None).increment(1);

        // `collapse worker where agg.worker` collapses only the series carrying that agg key.
        let policy: Policy = Policy::from_exprs(["collapse worker where agg.worker"]).unwrap();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        let cap = backend.inner();
        // The two worker series merged to one; `plain` passed through.
        assert!(cap.counters.contains(&("m".to_string(), None, 2)));
        assert!(cap.counters.contains(&("plain".to_string(), None, 1)));
        assert_eq!(cap.counters.len(), 2);

        // `name in (a, b)` is an OR of exact names.
        let registry = Registry::new();
        registry.register_counter("a".into(), None).increment(1);
        registry.register_counter("b".into(), None).increment(1);
        registry.register_counter("c".into(), None).increment(1);
        let policy: Policy = Policy::from_exprs(["drop where name in (a, b)"]).unwrap();
        let mut backend = super::super::Filtered::new(Capture::default(), policy);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        assert_eq!(names(backend.inner()), vec!["c"]);
    }

    #[test]
    fn quoted_value_allows_reserved_word() {
        // `and` as a literal tag value is fine when quoted.
        let m: Matcher = "level = 'and'".parse().unwrap();
        let name: std::sync::Arc<str> = std::sync::Arc::from("m");
        let tags = crate::tags::metric_tag_set([("level", "and")]);
        let mut info = MetricInfo::new(
            &name,
            None,
            Unit::Count,
            crate::backend::MetricKind::Counter,
        );
        info.tags = crate::MetricTags::new(&tags);
        assert!(super::super::Matcher::matches(&m, &info));
    }
}
