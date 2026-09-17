# Trimming the raw payload

Rev 1 keeps today's schema and drops the fields collimator and prism never
read. No restructuring, no renames, no new encoding — the same keys, fewer of
them, so neither consumer needs a change to read it.

`docs/example-report.json` is a real analysis under today's schema.
`docs/example-raw.json` is the same artifact under the rev-2 shape sketched at
the end, kept for reference.

## What the consumers actually read

Two services consume the payload, and their needs barely overlap.

**collimator** trains the model. It reads the report as a flat bag of files —
every archive member contributes, so an embedded file gives the same signal it
would as a standalone sample. Its whole feature pipeline goes through fifteen
accessors. Per file it needs `type`, `size`, `depth`, `mol`, `risk` and
`facts.metrics`; per trait, `id`, `crit` and `conf`. It reads nothing else:
not `desc`, `spans`, `ctx`, `refs`, `sha`, `pid` or `rel`.

**prism** renders the sample page and needs the structure collimator ignores:
`sha` for member links, `pid`/`rel`/`via`/`role` for the tree and dependency
panel, `ctx[].b` for the content view, and per trait `spans`, `uses` and
`from`. `uses` is the only source of bonds in its molecule drawing. It reads
no metrics at all.

## Rev 1: the same schema, fewer fields

| Field | Read by | Action |
| --- | --- | --- |
| `supp` | neither | **drop** |
| `loc` / `el` | prism, only when `spans` produced nothing | **drop** |
| `facts.str`, `facts.sec`, `facts.val` | neither | **drop** |
| `ctx[].col`, `ctx[].t` | prism parses, never reads | **drop** |
| `mbc`, `atk` | collimator knobs, default off | **drop**, see below |
| `desc` | prism, with a fallback | **drop**, see below |
| `refs` | prism intends to, blocked by a bug | keep |
| `ident` | prism | keep |
| `facts.imp/exp/funcs/tgt/mbr` | collimator `gap:` features | keep |
| everything else | one or both | keep |

Measured, zstd-19, which is what the worker posts and roughly what Postgres
stores:

| Sample | Today | Rev 1 keeping `desc` | Rev 1 dropping `desc` |
| --- | ---: | ---: | ---: |
| npm archive, 5 files, 231 traits | 7,864 | 7,629 (97%) | 6,074 (**77%**) |
| Mach-O binary, 1 file, 8 traits | 2,550 | 2,430 (95%) | 2,319 (90%) |

Two things follow. **`desc` is the whole saving**, and it scales with trait
count, so archives — most of the corpus — benefit most. And **everything else
together is worth about 3%**, because the provably-dead fields are mostly
absent from real payloads anyway.

### `desc`

Static per trait id, and the payload already names the traits bundle in `rev`,
so anything wanting the text can join on it. Prism does not need that join:
it already falls back to the trait id at all three sites that render a
description. Dropping `desc` needs no prism change and costs a cosmetic
downgrade — the id reads worse than the sentence.

If that downgrade is unacceptable, rev 1 keeps `desc` and saves 3% instead of
23%. That is the one judgement call in this proposal.

### `mbc` and `atk`

Prism never reads them. Collimator reads them only behind
`COLLIMATOR_MBC_ID_VOCAB` and `COLLIMATOR_ATTACK_CODE_NGRAMS`, both off by
default. Dropping them makes those two experiments impossible without a
rescan. Cheap to keep if that optionality is worth 1%.

## Two fields that must never be omitted

**`crit` and `conf` stay explicit, even at their defaults.** Both consumers
gate confidence at 0.65 and both read a missing value as 0.0. Omitting `conf`
when it equals cleave's 0.5 default — the obvious compaction — silently
discards every finding in both: collimator trains on empty vectors, prism
renders "0 traits". Neither errors. The same holds for `crit`, where a missing
value reads as 0 and falls below every threshold.

This is the one place where saving bytes breaks the system quietly.

## Why rev 1 does not restructure

The obvious next step is to intern repeated trait ids and descriptions into a
dictionary, or to key files by content hash instead of array position. Both
were built and measured:

| Variant | Raw | zstd-19 |
| --- | ---: | ---: |
| Today's schema | 47,596 | 7,864 |
| Trait dictionary | 36,400 | 7,534 |
| Content-addressed nodes and edges | 36,822 | 7,559 |

Interning cuts the document 42% uncompressed and **4% compressed**. The worker
posts zstd and Postgres compresses JSONB, so zstd is already doing that
deduplication, and doing it better. A schema break that buys 4% is not worth
making — especially when both consumers must keep their old-entry readers
either way, so a new shape is a second reader rather than a replacement.

## Defects this surfaced

Three, all pre-existing, all worth fixing independently.

**Collimator reads two keys the current schema does not emit.** It derives
nesting from `p`, a parent *path* string, and per-file times from `mt`.
Today's schema renames the parent to `pid` and emits no per-file mtime, so
`struct:max_nesting_depth_log`, `struct:inner_file_ratio`,
`agg:hostile_depth_weight` and both mtime features are silently always zero.
The model is training with those columns dead. Pointing them at `pid` and
`depth` is a small fix that recovers real signal.

**Prism drops `refs` in its own decoder.** The field is on the struct and has
a live consumer that labels dependency chips by declared PURL, but the
hand-written `UnmarshalJSON` omits it, so it is always nil and chips fall back
to the fetched URL — the exact thing that consumer exists to prevent. Its test
builds the struct directly, so nothing catches it. This is why `refs` stays in
rev 1: dropping it would make the bug permanent.

**Prism parses facts, strings, imports, exports and sections and reads none of
them.** Pure decode cost on every page render; deleting that decode path is
free once rev 1 stops sending the fields.

## Rev 2, when it is worth it

Not for size. For composability, which the measurements say is nearly free
(0.3% compressed) and which fixes real problems:

- **Nodes keyed by sha256** rather than array position, so the same member in
  two archives is one node and merging two payloads is a map union. Hopper's
  split-and-rejoin law becomes that union instead of a hand-maintained list of
  which keys survive compaction.
- **One edge list** for what are currently four encodings: archive members
  (`pid` + `rel`), fetched dependencies (`via`), declared references
  (`refs`), and decoded payloads (a `##base64@96` suffix inside a
  `!!`-delimited path). They are all "this file leads to that one, by some
  means, at some location". An absent target is how an unresolved dependency
  says so, without a sentinel.
- **Locations relative to the artifact**, never the analyzing host's
  filesystem, which today leaks into every path and makes one artifact produce
  different reports on different workers.

`docs/example-raw.json` is that shape, built from the same analysis.

## Out of scope

The `diff` judge carries isomer's envelope, 200 KB of the 240 KB report. It
nests its own cleave diff of both sides, so the report holds the artifact's
traits twice over. That envelope was built for a CI job reading one
comparison, not for a corpus row. Neither collimator nor prism reads it. It
needs the same treatment, separately.
