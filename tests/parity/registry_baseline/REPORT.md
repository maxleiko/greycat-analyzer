# Parity baseline — `greycat/apps/registry`

Comparison run on 2026-05-08 against `~/dev/datathings/greycat/apps/registry`
(12 modules, ~3.3k LoC of `.gcl`).

## Headline

| | TS reference (`greycat-lang lint`) | Rust port (`greycat-analyzer lint`) |
|---|---:|---:|
| Errors | 0 | 121 |
| Warnings | 0 | 103 |
| Total diagnostics | 0 | 224 |

Every diagnostic the Rust port emits is a **false positive** relative to the
TS reference (TS reports the project as clean). Captured outputs:
[ts.txt](ts.txt), [rust.txt](rust.txt).

## Bucketed root causes

Each bucket gets a sub-chunk in the proposed Phase 17 and a per-bucket
ratchet (count today → target 0).

### Bucket A — Member-flow `any` cascades (~96 errors, ~80% of error count)

All three sub-buckets cascade off `Expr::Member` / `Expr::Call` /
`Expr::Arrow` returning `any` from the per-module analyzer pass:

| Shape | Count | Example |
|---|---:|---|
| `value of type \`any\` is not assignable to parameter \`X\`` | 61 | [`registry.gcl:163`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/registry.gcl) |
| `return value of type \`any\` is not assignable to declared return type \`X\`` | 27 | [`registry.gcl:330`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/registry.gcl) |
| `if condition must be \`bool\`, got \`any\`` | 8 | [`migrate.gcl:212`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/migrate.gcl) |

**Closes via P16** (already in the ROADMAP — member-flow inference + node-deref
+ primitive receiver). Should mostly disappear once chunks 16.1–16.4 land.

### Bucket B — Over-eager unused-local / unused-param lints (~54 warnings)

| Shape | Count |
|---|---:|
| `unused parameter \`X\`` | 31 |
| `unused local \`X\`` | 23 |

Two compounding issues:

1. **String-interpolation idents are not lowered** — [`hir/types.rs:365`](../../greycat-analyzer-hir/src/types.rs) `StringExpr.value` stores the raw fragments concatenated; the `${expr}` interpolation parts are dropped before the resolver sees them. So a parameter / local referenced *only* inside a string template (e.g. `"${base_url}${src_prefix}/"` in [`migrate.gcl:27`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/migrate.gcl)) is flagged as unused.
2. **TS reference doesn't emit unused-local / unused-param at all by default.** The Rust port's `unused-local` / `unused-param` lints (P4.2) are stricter than parity. Even after fix (1), the lints would fire on genuinely-unused names that TS silently accepts — likely a default policy decision more than a bug.

**Proposed fix:**
- **B1**: lower string-interpolation `${expr}` parts as real expressions in the HIR so the resolver visits them. This removes the false-positive cascade.
- **B2**: gate `unused-local` / `unused-param` behind a CLI flag (e.g. `--lint-unused`) so the default `lint` matches TS's "I'll trust the user" policy, with the strict mode opt-in.

### Bucket C — `for-in` body never reaches the resolver (~12 warnings)

Critical lowering bug: [`hir/lower.rs:547`](../../greycat-analyzer-hir/src/lower.rs#L547) reads `child_by_field_name("iterator")` (which returns the *iterable expression*, not a param) and then asks for `child_by_field_name("name")` on it. That returns `None` for any expr, so the `?` short-circuits and `lower_stmt` drops the whole `for_in_stmt` from the HIR.

The grammar's [`for_in_stmt`](../../tree-sitter-greycat/grammar.js#L210) has `sepBy2(",", $.for_in_param)` for the params (no field name) and `field("iterator", $._expr)` for the iterable. The lowering needs to:

- Walk named children for `for_in_param` nodes (handles both `for (x in xs)` and the tuple form `for (i, x in xs)`).
- Update `ForInStmt` to carry a `Vec<Idx<Ident>>` for params so the tuple form is representable.

The cascade affects:
- 9× `unused private fn` for fns called only inside a for-in body (e.g. `migrate_branch` at [`migrate.gcl:67`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/migrate.gcl#L67) is called at line 63 inside `for (i, bf in branch_dirs) { ... }` but the resolver never sees the call).
- Some unused-local / unused-param hits in Bucket B also originate here.
- Likely contributes to Bucket A too (idents inside the for-in body don't get types).

**Proposed fix:** lower `for_in_stmt` correctly + extend `ForInStmt` for the tuple form. Also adds a unit test against `for (i, x in xs)`.

### Bucket D — Catch-param scope hole (1 warning)

`unresolved name 'ex'` on [`registry.gcl:628`](../../../home/leiko/dev/datathings/greycat/apps/registry/src/registry.gcl#L628). Lowering at [`hir/lower.rs:589`](../../greycat-analyzer-hir/src/lower.rs#L589) tries `child_by_field_name("name")` on the `_catch_param` node, but [`_catch_param`](../../tree-sitter-greycat/grammar.js#L170) is `seq("(", $.ident, ")")` with no `name` field. So `error_param` ends up `None` and the catch ident never gets a binding.

**Proposed fix:** walk the `_catch_param`'s named children for the `ident` instead of asking for a non-existent field.

### Bucket E — `@library` resolution incomplete (1 warning)

`@library('explorer'): library not found`. The library lives at `webroot/explorer/` not `lib/explorer/` (it's a frontend asset, not a code library). The Rust port's [`pragma_diagnostics`](../../greycat-analyzer-core/src/diagnostics.rs) only checks `<project>/lib/<name>/` and `<greycat_home>/lib/std/` — needs to also accept the webroot location, or downgrade the diagnostic when the library is referenced but doesn't have analyzable `.gcl` content.

**Proposed fix:** match the resolver's broader contract (Context::iter_gcl already short-circuits on the missing-lib case at load time without throwing). The pragma_diagnostics check should mirror that — only flag genuinely-unknown libs, not asset libs.

### Bucket F — Other unused warnings

| Shape | Count | Notes |
|---|---:|---|
| `unused private fn \`X\`` | 9 | All cascade off Bucket C (for-in) + Bucket B (string-interp). |
| `unused private enum \`X\`` | 2 | Need to investigate — possibly genuinely unused. |
| `unused private var \`X\`` | 1 | Same. |

After Buckets B + C land, this bucket should shrink to whichever entries remain genuine.

## Proposed Phase 17 chunking

If you sign off, I'd structure Phase 17 as:

- **17.1** Capture baseline + bucketing (this report). **Done** — committed under [tests/parity/registry_baseline/](.).
- **17.2** Bucket C: fix `for-in` lowering (S, but high impact). Probably eliminates 12+ false positives just by visiting the for-in bodies.
- **17.3** Bucket D: fix `catch (ex)` lowering (S, one-liner).
- **17.4** Bucket E: fix `@library` resolution to accept webroot locations (S).
- **17.5** Bucket B1: lower string-interpolation `${expr}` parts as real HIR expressions (M). Big-deal fix — affects type inference too, not just unused checks.
- **17.6** Bucket B2: gate `unused-local` / `unused-param` behind `--lint-unused`, default off, to match TS (S).
- **17.7** Re-run baseline; ratchet down. Target: ≤ 5 residual diagnostics, all genuine. Add a CI job to assert the diff size doesn't regress.

P16 (already planned) handles Bucket A separately. P17.2/17.3 should land before P16.4 (cross-module call return type) so the for-in fix doesn't surface additional `any` types after P16's call-on-member typing.

## How to refresh this baseline

```sh
# from the workspace root:
cd ~/dev/datathings/greycat/apps/registry
greycat-lang lint                           > .../tests/parity/registry_baseline/ts.txt
greycat-analyzer lint --format=compact project.gcl \
  | sed 's|/home/leiko/.*registry/||g'      > .../tests/parity/registry_baseline/rust.txt
```
