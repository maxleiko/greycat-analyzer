//! The `@permission` contract.
//!
//! `@permission("name")` on a fn / method gates an exposed API surface.
//! Two rules:
//!
//! 1. Every named permission must be **declared** somewhere in the
//!    project closure via a top-level `@permission("name", "desc");`
//!    pragma (collected into [`ProjectIndex::module_permissions`],
//!    which spans the std library and every `@include` / `@library`
//!    module). An undeclared name is a hard `unknown-permission`
//!    error.
//! 2. A permission only fires on an **exposed** function. Without a
//!    sibling `@expose`, the check never runs, so the permission is
//!    dead — an advisory `permission-without-expose` warning. The
//!    runtime builds fine either way; this is the analyzer surfacing
//!    intent, not a runtime rule.
//!
//! Arg shape: at least one string permission name. Zero args
//! (`@permission()`) builds at the runtime but names nothing, so it is
//! a no-op — flagged `pragma-missing-args` (warning). A non-string arg
//! (`@permission(42)`) is a hard error. Both are handled generically by
//! the driver's [`super::ArgSig::Variadic`] (`min: 1`) check.

use greycat_analyzer_hir::types::AnnotationArgKind;

use super::{ArgSig, ArgType, Contract, PragmaCtx, PragmaUse, Site, SiteRule};
use crate::analyzer::{SemanticDiagnostic, Severity};

pub(super) const CONTRACT: Contract = Contract {
    name: "permission",
    sites: &[SiteRule {
        site: Site::Fn,
        args: ArgSig::Variadic {
            min: 1,
            ty: ArgType::String,
        },
        check: Some(check),
    }],
};

fn check(u: &PragmaUse, siblings: &[PragmaUse], cx: &PragmaCtx, out: &mut Vec<SemanticDiagnostic>) {
    let index = cx.index;
    let mut named_permission = false;
    for arg in u.ann.args.iter() {
        let AnnotationArgKind::String(sym) = arg.kind else {
            // Non-string args are reported by the driver's arg-shape
            // check; nothing for the semantic pass to do.
            continue;
        };
        named_permission = true;
        let declared = index
            .module_permissions
            .values()
            .any(|perms| perms.contains(&sym));
        if !declared {
            let name = &index.symbols[sym];
            out.push(SemanticDiagnostic::structural(
                Severity::Error,
                "unknown-permission",
                format!(
                    "unknown permission '{name}' — declare it with a top-level pragma \
                     `@permission(\"{name}\", \"<description>\");`"
                ),
                arg.span.clone(),
            ));
        }
    }
    // `@permission()` with no named permission is a no-op — nothing to
    // warn about. Only flag a genuine permission that can never fire.
    if named_permission
        && !siblings
            .iter()
            .any(|p| &index.symbols[p.ann.name] == "expose")
    {
        out.push(SemanticDiagnostic::structural(
            Severity::Warning,
            "permission-without-expose",
            "`@permission` has no effect without `@expose` — this function is not exposed, \
             so the permission is never checked; add `@expose` or remove the permission"
                .to_string(),
            u.ann.byte_range.clone(),
        ));
    }
}
