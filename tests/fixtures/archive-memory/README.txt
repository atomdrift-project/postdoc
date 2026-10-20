Large archive member memory fixtures

Generate a corpus outside /tmp before running benchmarks:

  python3 tests/fixtures/archive-memory/make-corpus.py out/archive-memory-data

The default zero-run member is 8.5 GiB plus 113 bytes, with evidence markers
at the beginning, middle, and end. The dense member is 129 MiB plus 113 bytes
of deterministic pseudorandom data. A third archive stops after the readable
prefix of a member declaring 256 MiB. Generation streams through at most one
MiB of input; it writes only compressed archives. The sibling manifest records
all archive hashes and expected member hashes and lengths. Use --large-mib 129
for a quick correctness corpus; use the default for memory comparisons.

These reproduce the allocation pattern found in a Drupal image's bundled
PEAR Archive_Tar tests/big.tar.bz2: 6,631 compressed bytes contain an entirely
zero-filled 8.5 GiB regular member. A geometrically growing Vec reserves
16 GiB while reading it. The synthetic fixtures add nonzero evidence so a
memory optimization must preserve meaningful bytes around the zero runs.

Compare the same corpus and full reports with the worker benchmark in
scripts/benchmarks.txt. Measure cgroup memory, including anonymous mappings;
jemalloc heap profiles alone do not account for those mappings. Keep profiling
separate from timing, disable idle work, and compare full wall time as well as
peak RAM. A fixture result does not establish whole-service memory use.
