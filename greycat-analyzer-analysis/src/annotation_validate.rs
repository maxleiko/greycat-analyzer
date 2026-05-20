//! Hard-error validator for decl-attached annotation arguments.
//!
//! GreyCat pragmas accept only **compile-time computable** values:
//! primitive literals (`string` / `int` / `float` / `bool` / `char`
//! / duration / time / iso8601), `null`, and path-shaped references
//! that resolve to a type declaration or an enum variant. Anything
//! else (a call, arithmetic, an array / object literal, an instance
//! member access on a value, …) is captured as
//! [`AnnotationArg::Invalid`] at HIR-lower time; Paths that don't
//! resolve to a type / enum / variant land here too.
//!
//! Both shapes surface as `Severity::Error` `SemanticDiagnostic`s
//! with code `invalid-pragma-arg`. The CLI package gate refuses on
//! them so a stale-pragma artifact never reaches the C runtime.
//!
//! Not a lint: lints can be silenced via `// gcl-lint-off …` /
//! `@lint_off(...)`. `invalid-pragma-arg` must not be silenceable
//! because every consumer of the pragma section assumes args are
//! const-primitive.

use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::types::{AnnotationArg, Decl, Modifiers};

use crate::analyzer::{SemanticDiagnostic, Severity};
use crate::stdlib::ProjectIndex;

/// Walk every decl-attached `modifiers.annotations` in `hir` and
/// push one [`SemanticDiagnostic`] per non-const argument into
/// `out`. Two failure modes:
///   - [`AnnotationArg::Invalid`] — structurally non-const at HIR
///     lower time (calls, arithmetic, array / object literals,
///     instance member accesses, etc.).
///   - [`AnnotationArg::Path`] whose chain doesn't resolve against
///     `index` to a type / enum / enum-variant / known decl. The
///     chain is checked by `path_resolves`.
pub fn validate_annotation_args(
    hir: &Hir,
    index: &ProjectIndex,
    out: &mut Vec<SemanticDiagnostic>,
) {
    for (_, decl) in hir.decls.iter() {
        match decl {
            Decl::Fn(d) => push_for_modifiers(&d.modifiers, index, out),
            Decl::Type(d) => {
                push_for_modifiers(&d.modifiers, index, out);
                for attr_idx in d.attrs.iter() {
                    let attr = &hir.type_attrs[*attr_idx];
                    push_for_modifiers(&attr.modifiers, index, out);
                }
                // Methods come through the outer `hir.decls.iter()`
                // sweep as their own `Decl::Fn` entries — no need to
                // recurse into them here.
            }
            Decl::Enum(d) => push_for_modifiers(&d.modifiers, index, out),
            Decl::Var(d) => push_for_modifiers(&d.modifiers, index, out),
            Decl::Pragma(_) => {} // mod_pragmas are validated separately
        }
    }
}

fn push_for_modifiers(
    modifiers: &Modifiers,
    index: &ProjectIndex,
    out: &mut Vec<SemanticDiagnostic>,
) {
    for ann in modifiers.annotations.iter() {
        for arg in ann.args.iter() {
            let bad_range: Option<Range<usize>> = match arg {
                AnnotationArg::Invalid { start, end } => Some((*start as usize)..(*end as usize)),
                AnnotationArg::Path { chain, start, end } => {
                    if path_resolves(chain, index) {
                        None
                    } else {
                        Some((*start as usize)..(*end as usize))
                    }
                }
                _ => None,
            };
            if let Some(range) = bad_range {
                out.push(SemanticDiagnostic::structural(
                    Severity::Error,
                    "invalid-pragma-arg",
                    "pragma arguments must be constant primitive values \
                     (string / int / float / bool / char / duration / time / null) \
                     or path references to a type or enum variant — calls, \
                     arithmetic, array / object literals, instance member access, \
                     and unresolved names can't be stored on a pragma at \
                     compile time"
                        .to_string(),
                    range,
                ));
            }
        }
    }
}

/// Decide whether `chain` resolves to a compile-time constant
/// against the project's name tables. Length-1 chains must hit a
/// known type or a project-level decl; longer chains are accepted
/// when the first segment is a known type / module / decl name —
/// full multi-segment resolution against the `(module, enum,
/// variant)` graph is left for the gcp-emit stage, where stale
/// resolutions also fail the package gate.
fn path_resolves(chain: &[greycat_analyzer_core::Symbol], index: &ProjectIndex) -> bool {
    let Some(&head) = chain.first() else {
        return false;
    };
    if index.type_names.contains(&head) {
        return true;
    }
    if index.decl_locations.contains_key(&head) {
        return true;
    }
    // Multi-segment chains: the first segment may also be a module
    // stem (e.g. `core::DurationUnit::milliseconds` — `core` is not
    // in `type_names` but it's the module name of every loaded
    // stdlib file). Accept conservatively; the gcp encoder
    // re-resolves and emits its own diagnostic if the chain truly
    // dangles.
    chain.len() > 1
}
