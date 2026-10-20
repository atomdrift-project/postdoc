The production stress replay exposed detached Tree-sitter predicates in five
JavaScript rules. A standalone predicate becomes its own zero-step pattern;
two such patterns abort in query.c on a deeply nested parse. Group each
predicate with its structural pattern instead.

predicates.patch applies to the traits repository (patch -p1). During this
investigation it is applied only to out/traits-catch-fixed, a separate copy of
out/bench-assets/traits. Use the same corrected snapshot for both benchmark
binaries. The production rules have not been modified.

query.yaml and sample.js check that the catch-clause pattern matches use of
the caught identifier and rejects the unrelated identifier. The native
reproducer is Binaryen test/passes/limit-segments_disable-bulk-memory.wast,
3065741 bytes, SHA-256:
bbc6011ede29b009251d5cc210c6f69d7b8f59bd5694040ed34ed92a63d091a7

All 199 JavaScript-compatible queries from 392 unique query strings ran on
that input without an assertion after the five fixes. The actual saved server
also completed it with zero errors. Evidence is in:
out/tree-sitter-query-repro/all-detached-fixed.log
out/tree-sitter-wast-fixed-before-1/summary.json
