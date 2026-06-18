//! Wadler/Leijen-style document IR.
//!
//! The IR carries no source position or kind information — it's purely a
//! layout description. The renderer in [`crate::render`] walks the tree
//! and emits text, deciding flat-vs-broken per `Group`.
//!
//! Constructor helpers (`text`, `concat`, `group`, `indent`, `line`,
//! `softline`, `hardline`, `space`, `nil`) keep call sites readable.

/// Layout primitive.
#[derive(Debug, Clone)]
pub enum Doc {
    /// Empty — renders to nothing.
    Nil,
    /// Atomic text. Never contains a newline; multi-line content uses
    /// `Hard` between segments.
    Text(String),
    /// Breakable boundary. When the enclosing `Group` is laid out flat,
    /// renders as a single space if `space_if_flat` is true, otherwise
    /// as nothing. When broken, renders as a newline + current indent.
    Line { space_if_flat: bool },
    /// Mandatory line break — always renders as newline + current indent.
    /// Forces every enclosing `Group` to break.
    Hard,
    /// Increase indentation by one step for the inner doc. The renderer
    /// applies the step on every newline emitted from inside.
    Indent(Box<Doc>),
    /// Sequence of docs with no implicit separator.
    Concat(Vec<Doc>),
    /// Layout choice: try flat (lines as spaces / nothing), break if the
    /// group's flat width exceeds the remaining line width.
    Group(Box<Doc>),
    /// User-driven blank line. The renderer emits a newline + a blank
    /// line iff the previous output didn't already end on a blank line.
    /// Used to preserve user-intent vertical separation between top-level
    /// decls and statements.
    BlankLine,
    /// "If broken, append this content; if flat, omit it." The canonical
    /// use is a trailing comma before a closing `)` or `]`: present when
    /// the group has been broken (one arg per line), absent inline.
    IfBroken(Box<Doc>),
    /// Indent the inner doc by one step *only when the enclosing
    /// `Group` is in Break mode*. In Flat mode, behaves like the inner
    /// doc with no added indent. The canonical use is a chain root
    /// wrapping its post-softline content: the chain's indent step
    /// only matters once the chain breaks; if it stays flat (because
    /// an inner expandable absorbs the width), no extra indent is
    /// imposed on the leading content.
    IndentIfBroken(Box<Doc>),
    /// "Expandable child" — the inner doc renders normally but
    /// contributes **zero width** to the *enclosing* group's
    /// fits-check. The inner doc is expected to be a `Group` that can
    /// break independently. Used to wrap delimited blocks (`{...}`,
    /// `[...]`, call `(...)`) so that a parent chain or expression
    /// can stay flat even when the inner block has to break across
    /// lines.
    Expand(Box<Doc>),
}

impl Doc {
    /// Empty doc — renders to nothing.
    pub fn nil() -> Doc {
        Doc::Nil
    }

    /// Atomic text. Pass any `Into<String>` — no internal newlines.
    pub fn text(s: impl Into<String>) -> Doc {
        Doc::Text(s.into())
    }

    /// Breakable space — flat: ` `, broken: newline + indent.
    pub fn line() -> Doc {
        Doc::Line {
            space_if_flat: true,
        }
    }

    /// Breakable nothing — flat: ``, broken: newline + indent.
    pub fn softline() -> Doc {
        Doc::Line {
            space_if_flat: false,
        }
    }

    /// Mandatory line break.
    pub fn hardline() -> Doc {
        Doc::Hard
    }

    /// Single ASCII space (non-breakable).
    pub fn space() -> Doc {
        Doc::Text(" ".into())
    }

    /// Concatenate a sequence of docs in order.
    pub fn concat(parts: Vec<Doc>) -> Doc {
        if parts.len() == 1 {
            parts.into_iter().next().unwrap()
        } else if parts.is_empty() {
            Doc::Nil
        } else {
            Doc::Concat(parts)
        }
    }

    /// Wrap a doc in a group — renderer chooses flat vs. broken once.
    pub fn group(inner: Doc) -> Doc {
        Doc::Group(Box::new(inner))
    }

    /// Indent the inner doc by one step. Affects every line emitted from
    /// inside.
    pub fn indent(inner: Doc) -> Doc {
        Doc::Indent(Box::new(inner))
    }

    /// Render only when the enclosing `Group` is broken.
    pub fn if_broken(inner: Doc) -> Doc {
        Doc::IfBroken(Box::new(inner))
    }

    /// Indent the inner doc by one step only when the enclosing
    /// `Group` is in Break mode.
    pub fn indent_if_broken(inner: Doc) -> Doc {
        Doc::IndentIfBroken(Box::new(inner))
    }

    /// Wrap an expandable child whose width should not poison the
    /// enclosing group's fits-check.
    pub fn expand(inner: Doc) -> Doc {
        Doc::Expand(Box::new(inner))
    }

    /// User-driven blank line — collapsed if previous output already ended
    /// on a blank line, idempotent across consecutive `BlankLine`s.
    pub fn blank_line() -> Doc {
        Doc::BlankLine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_singleton_is_unwrapped() {
        let d = Doc::concat(vec![Doc::text("hi")]);
        assert!(matches!(d, Doc::Text(_)));
    }

    #[test]
    fn concat_empty_is_nil() {
        assert!(matches!(Doc::concat(vec![]), Doc::Nil));
    }
}
