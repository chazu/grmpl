//! Source positions and the diagnostics that carry them.
//!
//! Every token, top-level declaration and view atom records where it starts
//! ([`Pos`]: 1-based line and column, the column counted in chars). A parse or
//! compile error is a [`Diagnostic`] at the offending position, and its
//! [`render`](Diagnostic::render)ed form is what the `String`-returning entry
//! points (`Program::compile*`, `CompiledPackage::compile_with_catalog`) hand
//! back:
//!
//! ```text
//! 2:1: expected `)`, found `view`
//!   |
//! 2 | view x() { foo(a) yield a }
//!   | ^
//! ```
//!
//! The rendering leads with `line:col: `, so a caller that knows the file name
//! can prefix it ([`with_path`]) to get the conventional `path:line:col:` form.

use std::fmt;

/// A position in source text: 1-based line and column, the column in chars.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl Pos {
    pub const START: Pos = Pos { line: 1, col: 1 };

    pub fn new(line: u32, col: u32) -> Pos {
        Pos { line, col }
    }
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// A value and the position it starts at.
#[derive(Clone, PartialEq, Debug)]
pub struct Spanned<T> {
    pub node: T,
    pub pos: Pos,
}

/// An error at a position in the source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub pos: Pos,
    pub msg: String,
}

impl Diagnostic {
    pub fn new(pos: Pos, msg: impl Into<String>) -> Diagnostic {
        Diagnostic {
            pos,
            msg: msg.into(),
        }
    }

    /// The diagnostic as `[path:]line:col: msg`, followed by the source line
    /// it points into and a caret under its column. A position past the last
    /// line (an empty source) renders the header alone.
    pub fn render(&self, source: &str, path: Option<&str>) -> String {
        let mut out = match path {
            Some(path) => format!("{path}:{self}"),
            None => self.to_string(),
        };
        let Some(text) = source.lines().nth(self.pos.line.saturating_sub(1) as usize) else {
            return out;
        };
        let number = self.pos.line.to_string();
        let gutter = " ".repeat(number.len());
        // Pad with the line's own tabs so the caret lines up however they render.
        let pad: String = text
            .chars()
            .take(self.pos.col.saturating_sub(1) as usize)
            .map(|c| if c == '\t' { '\t' } else { ' ' })
            .collect();
        out.push_str(&format!(
            "\n{gutter} |\n{number} | {text}\n{gutter} | {pad}^"
        ));
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for Diagnostic {}

/// Name the file a rendered diagnostic came from: `line:col: …` becomes
/// `path:line:col: …`. An error that does not lead with a position (a store
/// failure, a grant mismatch) is returned unchanged.
pub fn with_path(error: &str, path: &str) -> String {
    let positioned = error.split_once(": ").is_some_and(|(head, _)| {
        head.split_once(':').is_some_and(|(line, col)| {
            [line, col]
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    });
    if positioned {
        format!("{path}:{error}")
    } else {
        error.to_owned()
    }
}

/// The 1-based position of each char index of `chars`, and of the index one
/// past the end.
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(chars: &[char]) -> LineIndex {
        let mut starts = vec![0];
        starts.extend(
            chars
                .iter()
                .enumerate()
                .filter(|(_, &c)| c == '\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { starts }
    }

    pub(crate) fn pos(&self, index: usize) -> Pos {
        let line = self.starts.partition_point(|&start| start <= index);
        let col = index - self.starts[line - 1] + 1;
        Pos::new(line as u32, col as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_header_line_and_caret() {
        let src = "rel foo(a: Ent\nview x() { foo(a) yield a }\n";
        let d = Diagnostic::new(Pos::new(2, 6), "boom");
        assert_eq!(
            d.render(src, Some("w.grmpl")),
            "w.grmpl:2:6: boom\n  |\n2 | view x() { foo(a) yield a }\n  |      ^"
        );
        assert_eq!(
            Diagnostic::new(Pos::new(9, 1), "eof").render(src, None),
            "9:1: eof"
        );
    }

    #[test]
    fn with_path_only_prefixes_positioned_errors() {
        assert_eq!(with_path("3:14: bad", "w.grmpl"), "w.grmpl:3:14: bad");
        assert_eq!(with_path("store: gone", "w.grmpl"), "store: gone");
        assert_eq!(
            with_path("cannot listen on 1:2", "w.grmpl"),
            "cannot listen on 1:2"
        );
    }

    #[test]
    fn line_index_counts_chars_from_one() {
        let chars: Vec<char> = "ab\nçd\n".chars().collect();
        let index = LineIndex::new(&chars);
        assert_eq!(index.pos(0), Pos::new(1, 1));
        assert_eq!(index.pos(2), Pos::new(1, 3));
        assert_eq!(index.pos(4), Pos::new(2, 2));
        assert_eq!(index.pos(6), Pos::new(3, 1));
    }
}
