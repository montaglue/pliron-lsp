//! Renaming local names: SSA values (results and block arguments) and
//! block labels. A new name must not clash with another name of the same
//! pliron name scope (values: the enclosing `IsolatedFromAbove` op; labels:
//! the region). `@symbols` are renamed across files, see
//! [`crate::workspace::symbol_occurrences`].

use pliron_ir_syntax::Encoding;

use super::entity::{Entity, EntityKind};
use crate::document::Document;
use crate::exact::Range;

/// The ranges to replace to rename the value or block `e` to `new`, or why
/// it cannot be renamed.
pub fn local_rename(
    doc: &Document,
    e: &Entity,
    new: &str,
    enc: Encoding,
) -> Result<Vec<Range>, String> {
    if e.name != new
        && let Some(other) = conflict(doc, e, new)
    {
        let what = match e.kind {
            EntityKind::Block => "block",
            _ => "value",
        };
        let line = doc.position(other.0, enc).line + 1;
        return Err(format!(
            "`{new}` already names another {what} in this scope (line {line})"
        ));
    }
    Ok(e.occurrences())
}

/// The definition `e` would clash with if renamed to `new`.
fn conflict(doc: &Document, e: &Entity, new: &str) -> Option<Range> {
    if e.exact
        && let Some(x) = doc.fresh_exact()
    {
        return match e.kind {
            EntityKind::Value => x.value_at(e.at.0).and_then(|v| x.value_conflict(v, new)),
            EntityKind::Block => x.block_at(e.at.0).and_then(|b| x.block_conflict(b, new)),
            _ => None,
        };
    }
    let a = &doc.syntax;
    let tok = a.tree.token_at(e.def?.0)?;
    let other = a.conflicting_def(a.def_of_token(tok)?, new)?;
    let t = a.tree.tok(a.def(other).tok);
    Some((t.start, t.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::entity::entity_at;

    const SRC: &str =
        "builtin.module @m {\n^e():\n  t.f @a {\n  ^b(x: i):\n    y = t.c;\n    t.r x, y\n  }\n}\n";

    fn at(needle: &str) -> u32 {
        SRC.find(needle).unwrap() as u32
    }

    #[test]
    fn renames_values_and_refuses_clashes() {
        let doc = Document::new(SRC.into(), 0, &Default::default());
        let x = entity_at(&doc, at("x: i")).unwrap();
        let ranges = local_rename(&doc, &x, "w", Encoding::Utf16).unwrap();
        assert_eq!(ranges.len(), 2);
        let err = local_rename(&doc, &x, "y", Encoding::Utf16).unwrap_err();
        assert!(err.contains("line 5"), "{err}");
        // Renaming to its own name is fine.
        assert!(local_rename(&doc, &x, "x", Encoding::Utf16).is_ok());
        let b = entity_at(&doc, at("^b")).unwrap();
        assert!(local_rename(&doc, &b, "e", Encoding::Utf16).is_ok());
    }
}
