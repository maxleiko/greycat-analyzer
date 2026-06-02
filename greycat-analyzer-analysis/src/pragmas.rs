//! GreyCat language-pragma contract validation — the framework.
//!
//! GreyCat pragmas carry implicit contracts: where each may appear
//! (a fn, a type attribute, a type, …), how many arguments of what
//! type, plus semantic rules (a `@permission` usage must name a
//! permission declared somewhere in the project). Each pragma's contract is a
//! declarative [`Contract`] validated by one uniform driver.
//!
//! This file is the **framework** — the contract vocabulary
//! ([`Site`] / [`Contract`] / [`SiteRule`] / [`ArgSig`] / [`ArgType`])
//! plus the [`validate_pragmas`] driver. Each concrete pragma's
//! contract and semantic hook lives in its own submodule
//! (`pragmas/permission.rs`, …) and contributes one [`Contract`] to
//! [`CONTRACTS`].
//!
//! This is **not** a configurable lint. Like
//! [`crate::annotation_validate`]'s `invalid-pragma-arg`, hard
//! violations surface as [`Severity::Error`] [`SemanticDiagnostic`]s
//! (non-silenceable — the package gate refuses on them); advisory
//! findings surface as [`Severity::Warning`].
//!
//! Distinct from [`crate::meta_pragmas`], which handles the analyzer's
//! own `@lint_off` / `@lint_on` directives (CST-based, pre-HIR, lint
//! policy). Those aren't GreyCat language pragmas — the runtime has
//! never heard of them. The pragmas validated here are runtime-
//! meaningful and need the lowered HIR plus the full [`ProjectIndex`].

mod permission;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::types::{Annotation, AnnotationArgKind, Decl, Modifiers};

use crate::analyzer::{SemanticDiagnostic, Severity};
use crate::stdlib::ProjectIndex;

/// The registry. One entry per known language pragma; each lives in
/// its own submodule.
const CONTRACTS: &[Contract] = &[permission::CONTRACT];

fn contract_for(name: &str) -> Option<&'static Contract> {
    CONTRACTS.iter().find(|c| c.name == name)
}

/// Where a pragma / annotation is attached. Selects which per-site
/// rule of a [`Contract`] applies.
///
/// Payload-free today. When type-conditional pragmas land (`@format`
/// keys on whether the attribute is `time` / `duration`; `@expose`
/// keys on whether a method is `static`), `Attr` / `Fn` grow the
/// resolved type / static-ness, and the arg-shape vocabulary grows to
/// match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Fn,
    Attr,
    Type,
    Enum,
    Var,
}

/// A normalized pragma use. Wraps the HIR [`Annotation`], which already
/// carries the name, the per-argument values and spans, and the
/// whole-annotation span. A newtype rather than a bare `&Annotation`
/// so it can grow site-resolved context (attr type, static-ness)
/// without churning hook signatures.
struct PragmaUse<'a> {
    ann: &'a Annotation,
}

/// Context the semantic hooks need beyond the use itself.
struct PragmaCtx<'a> {
    index: &'a ProjectIndex,
}

/// A pragma's complete contract: which sites it is valid on, and the
/// rule (arg shape + optional semantic check) at each.
struct Contract {
    name: &'static str,
    sites: &'static [SiteRule],
}

/// Hook for the semantic / cross-pragma validation a declarative arg
/// shape can't express. Runs after the arg shape is checked, and
/// receives the sibling pragmas at the same site so it can reason
/// about relations like "permission needs expose".
type CheckFn = fn(&PragmaUse, &[PragmaUse], &PragmaCtx, &mut Vec<SemanticDiagnostic>);

struct SiteRule {
    site: Site,
    args: ArgSig,
    check: Option<CheckFn>,
}

/// Argument-shape rule. Only `Variadic` exists today — the bounded /
/// positional / overloaded / attr-type-conditional shapes (`Exact`,
/// `Params`, `OneOf`, …) land with their first real contract.
enum ArgSig {
    /// `min` or more arguments, each of type `ty`. Fewer than `min`
    /// is an advisory warning (the construct is a no-op, e.g.
    /// `@permission()` names no permission); a wrong-typed arg is a
    /// hard error (the runtime rejects it).
    Variadic { min: u8, ty: ArgType },
}

#[derive(Clone, Copy)]
enum ArgType {
    String,
}

impl ArgType {
    fn label(self) -> &'static str {
        match self {
            ArgType::String => "string",
        }
    }

    fn matches(self, kind: &AnnotationArgKind) -> bool {
        match self {
            ArgType::String => matches!(kind, AnnotationArgKind::String(_)),
        }
    }
}

/// Validate every decl-attached pragma in `hir` against the contract
/// registry, pushing diagnostics into `out`.
pub fn validate_pragmas(hir: &Hir, index: &ProjectIndex, out: &mut Vec<SemanticDiagnostic>) {
    for (_, decl) in hir.decls.iter() {
        for (site, modifiers) in decl_sites(hir, decl) {
            check_site(site, modifiers, index, out);
        }
    }
}

/// The (site, modifiers) pairs a decl contributes. A `type` carries
/// its own annotations *and* one Attr site per attribute. Methods
/// arrive through the outer `hir.decls` sweep as their own `Decl::Fn`,
/// so no recursion into them here.
fn decl_sites<'a>(hir: &'a Hir, decl: &'a Decl) -> Vec<(Site, &'a Modifiers)> {
    match decl {
        Decl::Fn(d) => vec![(Site::Fn, &d.modifiers)],
        Decl::Type(d) => {
            let mut sites = vec![(Site::Type, &d.modifiers)];
            for attr in d.attrs.iter() {
                sites.push((Site::Attr, &hir.type_attrs[*attr].modifiers));
            }
            sites
        }
        Decl::Enum(d) => vec![(Site::Enum, &d.modifiers)],
        Decl::Var(d) => vec![(Site::Var, &d.modifiers)],
        Decl::Pragma(_) => Vec::new(),
    }
}

fn check_site(
    site: Site,
    modifiers: &Modifiers,
    index: &ProjectIndex,
    out: &mut Vec<SemanticDiagnostic>,
) {
    if modifiers.annotations.is_empty() {
        return;
    }
    // Build the sibling set once so hooks can see the other pragmas at
    // this site (e.g. permission asking "is there an @expose here?").
    let uses: Vec<PragmaUse> = modifiers
        .annotations
        .iter()
        .map(|ann| PragmaUse { ann })
        .collect();
    let cx = PragmaCtx { index };
    for u in &uses {
        let name = &index.symbols[u.ann.name.symbol];
        let Some(contract) = contract_for(name) else {
            // Unregistered pragma — ignored for now. A future
            // "unexpected pragma" catch-all (once the registry covers
            // every valid pragma) slots in here.
            continue;
        };
        let Some(rule) = contract.sites.iter().find(|r| r.site == site) else {
            // Registered, but not valid at this site. A future
            // "pragma not valid here" diagnostic slots in here.
            continue;
        };
        validate_args(u, &rule.args, name, out);
        if let Some(check) = rule.check {
            check(u, &uses, &cx, out);
        }
    }
}

fn validate_args(u: &PragmaUse, sig: &ArgSig, pragma: &str, out: &mut Vec<SemanticDiagnostic>) {
    match sig {
        ArgSig::Variadic { min, ty } => {
            // Too few args — advisory. The runtime tolerates e.g.
            // `@permission()`, but it names nothing, so the annotation
            // does nothing.
            if u.ann.args.len() < *min as usize {
                let plural = if *min == 1 { "" } else { "s" };
                out.push(SemanticDiagnostic::structural(
                    Severity::Warning,
                    "pragma-missing-args",
                    format!(
                        "`@{pragma}` has no effect with no arguments — it expects at least \
                         {min} {} argument{plural}",
                        ty.label()
                    ),
                    u.ann.name.byte_range.clone(),
                ));
            }
            // Wrong-typed arg — hard error, matching the runtime.
            for arg in u.ann.args.iter() {
                if !ty.matches(&arg.kind) {
                    out.push(SemanticDiagnostic::structural(
                        Severity::Error,
                        "pragma-arg-type",
                        format!("`@{pragma}` expects {} arguments", ty.label()),
                        arg.span.clone(),
                    ));
                }
            }
        }
    }
}
