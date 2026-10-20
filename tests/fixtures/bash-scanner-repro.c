#include <stdio.h>
#include <stdlib.h>

// Compile with -I pointing at tree-sitter-bash's src directory. Including the
// scanner exposes its private token enum so this test exercises the exact
// external-scanner code shipped by the dependency.
#include "scanner.c"

typedef struct {
    TSLexer lexer;
    const int32_t *input;
    size_t length;
    size_t offset;
} TestLexer;

static void test_advance(TSLexer *lexer, bool skip) {
    (void)skip;
    TestLexer *test = (TestLexer *)lexer;
    if (test->offset < test->length) {
        test->offset++;
    }
    lexer->lookahead = test->offset < test->length ? test->input[test->offset] : 0;
}

static void test_mark_end(TSLexer *lexer) { (void)lexer; }
static bool test_eof(const TSLexer *lexer) {
    const TestLexer *test = (const TestLexer *)lexer;
    return test->offset >= test->length;
}

static void run_case(const char *name, const int32_t *input, size_t length,
                     bool expected) {
    TestLexer test = {
        .lexer = {.lookahead = input[0],
                  .advance = test_advance,
                  .mark_end = test_mark_end,
                  .eof = test_eof},
        .input = input,
        .length = length,
    };
    bool valid_symbols[ERROR_RECOVERY + 1] = {0};
    valid_symbols[BRACE_START] = true;
    void *scanner = tree_sitter_bash_external_scanner_create();
    bool result = tree_sitter_bash_external_scanner_scan(scanner, &test.lexer,
                                                         valid_symbols);
    tree_sitter_bash_external_scanner_destroy(scanner);
    printf("%s: result=%d expected=%d consumed=%zu\n", name, result,
           expected, test.offset);
    if (result != expected) exit(2);
}

int main(void) {
    const int32_t ascii_range[] = {'{', '1', '.', '.', '9', '}'};
    const int32_t high_codepoint_first[] = {'{', 0x10ffff, '.', '.', '9', '}'};
    const int32_t high_codepoint_second[] = {'{', '1', '.', '.', 0x10ffff, '}'};
    run_case("ascii-range", ascii_range, 6, true);
    run_case("unicode-first", high_codepoint_first, 6, false);
    run_case("unicode-second", high_codepoint_second, 6, false);
}
