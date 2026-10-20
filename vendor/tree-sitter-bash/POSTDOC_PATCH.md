# Local Bash scanner patch

This is the MIT licensed `tree-sitter-bash` 0.25.1 crate distributed by
crates.io. Postdoc patches that crate through `[patch.crates-io]` because its
external scanner passes Tree-sitter's Unicode `lexer->lookahead` directly to
the byte-oriented C `isdigit` macro. A high code point after `{` or `..` in a
brace range can read outside libc's character table and crash the worker.

The only upstream source change is in `src/scanner.c`: the two brace-range
digit tests now accept ASCII `0`–`9` explicitly. The dependency lock records
the patched source, which also changes Filefacts' build fingerprint. If the
scanner is changed again, update the package version or the Filefacts build
fingerprint so persisted parse caches cannot reuse old results.

The focused regression is `tests/fixtures/bash-scanner-repro.c` in Postdoc.
Run from the Postdoc root with:

```sh
cc -O2 -Ivendor/tree-sitter-bash/src tests/fixtures/bash-scanner-repro.c -o /tmp/bash-scanner-repro
/tmp/bash-scanner-repro
```
