# postdoc

Status: in progress (2026-09-16). Phase 1 is under way; see
[Phases](#phases) for what is built. Everything not marked there is still a
plan.

postdoc replaces `atomscan serve` and `atomscan worker`. It is the process that
sits between hopper and the analyzers: it claims samples, judges them, and
posts a verdict. Its one HTTP caller is beamline. Scan, cleave, and isomer
keep their library surfaces and their one-shot CLIs, and scan's own serve and
worker modes stay untouched and deployable until postdoc has rolled out.

The one new capability is differential analysis. When a sample has a PURL,
hopper names an earlier release of the same package that it already holds.
postdoc fetches that release and runs isomer over the pair, so the verdict
can say what this release *added*, not only what it contains.

## Principle: postdoc grades nothing

Every severity in the output comes from the engine that produced it. The
trait floor rule is scan's. The firing level is azoth's, read through scan.
The per-file LLM blend, including whether and how far it may loosen, is
scan's interpreter. The differential severity, its band mapping, and the
diff LLM's authority are isomer's. postdoc owns two things only: the order
the judges run in and the `max` that folds the two engines' outcomes. It has
no thresholds, no trait lists, no prompts, and no opinion about LLMs.

## Judges

A judge is one engine with one opinion. Five run:

| Key        | Engine                        | Opinion                                     | Folded by |
|------------|-------------------------------|---------------------------------------------|-----------|
| `cleave`   | cleave traits + scan's floor  | floor: hostile / suspicious / none          | scan      |
| `ml`       | azoth via scan                | firing level (`fires_at`)                   | scan      |
| `llm`      | scan's per-file interpreter   | benign / suspicious / hostile, scan's bound | scan      |
| `diff`     | isomer over (baseline, new)   | isomer severity, mapped to a band by isomer | isomer    |
| `diff_llm` | isomer's per-diff interpreter | benign / suspicious / malicious             | isomer    |

`cleave` is the rule scan today calls the trait floor (`apply_trait_floor` in
scan's engine): a model-benign file is raised to hostile when confident
crit-5 traits are corroborated across enough trait families, or to suspicious
when confident crit-4 traits span enough families. postdoc surfaces it as its
own judge instead of leaving it folded silently into the ML level.

`ml` is the raw azoth decision, before the floor. Its `fires_at` is the
tightest false-positive budget per 100M benign files at which the file grades
hostile, `-1` when it never fires, `null` in manual-threshold mode. This is
the number beamline, scan's `/v1/lookup`, and hopper's `/v1/lookup` already
gate on.

`diff` runs only when a baseline exists (see Baseline). Isomer already maps
model classes onto its five-level scale (`Risk::model_severity`: suspicious
is High, hostile is Critical); the inverse mapping, Critical to hostile and
High to suspicious, is added to isomer and postdoc calls it.

`llm` and `diff_llm` share one endpoint configuration, one circuit breaker,
and one concurrency budget. What each may do to a verdict is its engine's
rule, not postdoc's: scan's interpreter moves its outcome by at most one
band in either direction, and isomer's interpreter does whatever isomer
decides it may. postdoc reports both opinions and folds neither itself.

## Combining

Severity is the three-band scale scan already uses: benign, suspicious,
hostile. The level is the `fires_at` number that encodes it, so a consumer
that only reads a number still sees the combined verdict.

Two engines each produce one outcome, and postdoc keeps the worse:

1. **scan's outcome** is the per-file verdict as scan's engine already
   computes it: the azoth decision, raised by the trait floor, then blended
   with the interpreter under scan's own bound. This is `Interpretation.
   outcome` when the interpreter ran and the floored decision otherwise. The
   `cleave`, `ml`, and `llm` judge objects show the pieces; scan folds them.
2. **isomer's outcome** is `Report.verdict.severity` mapped through
   `Severity::band`, with isomer's interpreter already applied by isomer's
   rules. The `diff` and `diff_llm` judge objects show the pieces; isomer
   folds them. When there is no baseline there is no isomer outcome.
3. `final = max(scan, isomer)`. The level is the outcome's `fires_at`; a
   band that came from isomer takes its level from scan's `interpreted_level`,
   the function scan's interpreter uses to place a verdict inside a band.

`decided_by` lists the judges whose severity equals the final one, so a
reader can tell a model verdict from a floor verdict from a diff verdict.

## Baseline

The baseline is the release postdoc diffs against. Hopper chooses it, because
hopper is the only party that knows what it holds. The rule is the
version-drift predicate hopper already has, generalized:

> the newest earlier top-level sample with the same `purl_base`, a different
> version, an analysis on file, no `skip`, and `label <> 'bad'`; failing
> that, the newest earlier analyzed sample of any label.

"Earlier" is by `created_at`, not by parsed version, for the reason hopper's
triage code gives: there is no version grammar shared across ecosystems, and
arrival order answers the question without inventing one.

A convicted predecessor is a valid baseline. Isomer already handles the case
where the old side is itself hostile, and the baseline's label and level are
in the judge output so a reader can see it. When no baseline exists, `diff`
and `diff_llm` are `skipped` with reason `no_baseline`.

The registry is not consulted for a predecessor in this version. Hopper is
the corpus; a registry fallback can be added later without changing the wire
shape.

## Result JSON

One object per analyzed sample. Every judge key is always present. A judge
that did not run is `{"status": "skipped", "reason": "..."}`; one that failed
is `{"status": "error", "reason": "..."}`. Consumers write one code path.

```json
{
  "v": "1",
  "sha256": "…",
  "purl": "pkg:npm/left-pad@1.3.1",
  "baseline": { "sha256": "…", "purl": "…", "version": "1.3.0",
                "label": "good", "fires_at": -1 },
  "verdict": {
    "severity": "hostile",
    "fires_at": 25,
    "reason": "postinstall fetches and executes a remote script",
    "findings": [ { "id": "…", "crit": 5, "desc": "…", "file": "…" } ],
    "decided_by": ["ml", "diff"],
    "engines": { "postdoc": "0.1.0", "scan": "2.12.0", "cleave": "2.12.0",
                 "isomer": "0.5.0", "traits": "abc12345", "model": "…" },
    "analyzed_at": "2026-09-15T12:00:00Z",
    "duration_ms": 4120
  },
  "cleave":   { "status": "ok", "severity": "benign", "fires_at": null,
                "duration_ms": 900, "version": "abc12345",
                "raw": { …cleave CompactReport… } },
  "ml":       { "status": "ok", "severity": "hostile", "fires_at": 25,
                "confidence": 0.93, "duration_ms": 80, "version": "…",
                "raw": { …scan MlSection… } },
  "llm":      { "status": "ok", "severity": "hostile", "confidence": 0.95,
                "duration_ms": 2100, "version": "qwen3-…",
                "raw": { …scan Interpretation… } },
  "diff":     { "status": "ok", "severity": "hostile", "fires_at": 25,
                "duration_ms": 1300, "version": "0.5.0",
                "raw": { …isomer envelope… } },
  "diff_llm": { "status": "skipped", "reason": "no_baseline" }
}
```

The judge envelope is the same for all five: `status`, `reason` (only when
not ok), `severity` in postdoc's three bands, `fires_at`, `confidence` (0..1
where the engine has one), `duration_ms`, `version`, and `raw`. `raw` is the
engine's native output, verbatim and untrimmed: the cleave `CompactReport`,
scan's `MlSection` and `Interpretation`, isomer's `Report` and
`Interpretation`. No judge carries anything beside `raw`: the baseline sits at the top level
of the result, because it describes what was analyzed rather than what any
engine concluded.

Raw output lives inside the judge that produced it. That is the right
placement: each judge object is self-describing, a consumer learns one rule
("the summary is the envelope, the evidence is `raw`") instead of matching
top-level keys to judges, and no top-level field has ambiguous provenance.
The cleave report is also the input to `ml` and `llm`, but it is cleave's
output, and that is where it sits. The cost is that hopper's result parser
reads `cleave.raw` instead of `raw`; hopper is changing anyway.

`verdict` is scan's `V1Decision` without `decision`. `decision`
(allow/block/unanalyzed/unavailable) depends on the caller's false-positive
budget, so `/v1/lookup` computes it at answer time exactly as today and it is
never stored. `verdict.findings` follows scan's existing rule (crit 4 and up,
worst first, at most three) and `verdict.reason` is the interpreter's
sentence, so beamline no longer has to reconstruct either from `raw`.

Size is governed by `full=1`, the flag beamline already sends: without it the
serve routes strip every `raw`; with it they return the whole object. Posts
to hopper always carry everything, under hopper's existing body cap, and
when over it postdoc drops `cleave.raw` first, as scan's worker does today.

## Modes

**worker** is the hopper pull loop. postdoc does not copy it and scan does
not lose it: scan's `worker::run` takes a backend, `fn analyze(&self, job:
&Job) -> Result<impl Serialize>`, and scan's default backend is what
`atomscan worker` runs today. postdoc supplies a backend that runs the judges
and returns the result object. Claim, download, provenance, memory admission,
cleave gate, spooling, heartbeat, lease renewal, release, retry, dependency
mirroring, and the two-phase LLM renewal are all unchanged. Hopper spawns its
local worker as `atomscan worker --url … --name local [--data-dir …]
[--max-rss-gb …] [--workers …] [--interpret]` with `SCAN_LLM`,
`CLEAVE_VALIDATE_SOFT`, and `TMPDIR` set; `postdoc worker` accepts that argv
verbatim, so the binary name is the only thing hopper's config changes.

**serve** is a drop-in replacement for `atomscan serve`. Every route scan
answers, postdoc answers the same way: same paths, same request shapes, same
status codes, same error codes, same `X-Scan-Source` header. Swapping the
binary is a deployment change and nothing else.

| Route | |
|---|---|
| `GET  /v1/lookup` | The decision API. |
| `POST /v1/analyze` | Analyze and answer; NDJSON progress frames, then the decision. |
| `POST /analyze`, `/analyze-purl`, `/analyze-path` | Upload, coordinate, and loopback-only path. |
| `GET  /lookup`, `/status` | The legacy lookup and the reconnect probe. |
| `GET  /_/health`, `/_/info`, `/_/stats` | Liveness, build facts, counters. |
| `GET  /_/memory`, `/_/requests`, `/_/threads` | Operator diagnostics. |
| `POST /_/reload`, `/_/update` | Hot-swap the bundles. |

An earlier draft cut this to the four routes beamline calls. That was wrong
twice over. Beamline is the only *application* caller, but the `/_/*` routes
have operators and deploy scripts behind them, and a survey that finds no
code calling a route has not shown that nobody does. More importantly, a
drop-in cannot be a subset: the point of the swap is that nothing else has to
change at the same time.

**`/v1` does not move.** It is scan's contract and postdoc inherits it
unchanged. New fields may appear — a JSON consumer ignores what it does not
know, and beamline spreads the object it receives — but no field changes
meaning, changes type, or disappears. `/v1/analyze?full=1` keeps returning
`{ml, llm, raw}`, because beamline tests for `ml.eng` and `raw` to decide
whether it holds a full envelope.

**`/v2` is where the result object lives.** The five judges, the baseline,
`decided_by` and everything else this design adds are a different shape, not
an extension of the old one, so they get a different version. `/v2/lookup`
and `/v2/analyze` answer with it. Nothing is obliged to move: `/v1` stays as
long as it has callers, and `/v2` is opt-in per caller rather than a
migration with a date on it.

This changes how the server gets built. The earlier draft had postdoc writing
a small server against the `Analyzer` API. Byte-compatibility with `/v1` is
not something to re-derive from a specification — it is a hundred small
behaviours, each of which is a bug if it differs: which `X-Scan-Source`
values are emitted, that `?url=` alone always answers 404, the
`X-Hopper-Fresh` header alias, the 50-key limit, single-flight followers
replaying a leader's rendered error, the 250 ms grace before progress frames
begin. So scan's server modules move to postdoc substantially intact, and
`/v2` is added beside them. The handlers are 267 KB and get split by route
family on the way, but they are moved, not rewritten.

There is no one-shot CLI. `atomscan` and `isomer` remain the tools for that.

## Hopper changes

1. **Claims carry the baseline.** `ClaimJob` gains `purl` (built with
   `pkgparse.SourcePURL` from the row's identity columns, as `/v1/lookup`
   does) and an optional `baseline` object `{sha256, purl, version, path,
   label, fires_at, analyzed_at}`, filled by one batch query at claim time
   the way `ShasWithProvenance` fills `has_provenance`. `path` is relative to
   the data root so a co-located worker reads bytes off disk; otherwise it
   fetches `/api/file/{sha256}`.
2. **`GET /api/baseline?purl=`** answers the same query for one PURL, for
   serve-mode analyses that arrive by PURL rather than by claim. 200 with the
   baseline object, 204 when none.
3. **`POST /api/result` takes the result object.** The body is the object
   above with `worker` and `error` added. Hopper reads `verdict` for its
   level, engine, timestamp, and reason; reads `cleave.raw` where it reads
   `raw` today, with the same depth-0 `sha` check; and stores `verdict` and
   the five judge objects in two new columns, `verdict JSONB` and
   `judges JSONB`. The legacy `{ml, llm, raw}` body keeps working for as
   long as scan workers post it; readers prefer `verdict` when present.

`/v1/lookup` on hopper is unchanged. `litmus.go` gains the binary name as a
setting; the argv does not change.

## Beamline changes

Beamline's `isFullEnvelope`, `llmWhy`, and `topHits` read `ml.eng`,
`llm.interpretation`, and `raw.files[].traits`. With the result object they
become reads of `verdict.engines`, `verdict.reason`, and `verdict.findings`,
which is less code and drops beamline's own copy of the findings filter.
Nothing else in beamline's use of scan changes.

## Scan changes

Scan keeps everything it has. These are additions and one refactor, and
`atomscan serve` and `atomscan worker` keep working throughout.

- **One daemon entry point.** `Analyzer::scan(&self, Subject) ->
  Result<ScanResult>`, where `Subject` is `{source: Path | Bytes, label,
  registry: Option<RegistryProvenance>, follow: FetchPolicy, extract_dir,
  cancellation, phase}`. Scan's server calls it in place of its ten-argument
  `classify_file` and `classify_bytes`, which become private wrappers, so
  there is one analysis path.
- **The trait floor is a pure function.** `trait_floor(findings,
  active_level, grid_max) -> Option<FloorDecision>`, public, and `ScanResult`
  carries the raw model decision and the floor outcome separately. Scan's own
  paths apply the floor as they do now, so nothing scan emits changes.
- **The worker takes a backend.** `worker::run(config, backend)`, with
  scan's current behavior as the default backend.
- **Small server modules become public.** `acl`, `access`, `flight`,
  `corpus`, `decision`, `idle`, `latency`, `lookup`, and the `/_/stats`
  counters are each self-contained and are reused by postdoc's server as-is.
- `MlSection` and `Interpretation` gain `Deserialize`.

## Isomer changes

Isomer is binary-only today and every type is crate-private. postdoc needs it
as a library.

- Add `src/lib.rs`. `Severity`, `Format`, and `Gate` move out of `main.rs`;
  `main.rs` keeps the clap derive and builds an `Options` struct the library
  takes in place of `&Cli`. `analysis.rs` reads nine fields off `Cli`
  (`fail_on`, `gate`, `offline`, `no_follow`, `deps`, `llm`, `llm_timeout`,
  `format`, `progress`); those are `Options`.
- `printable` and `clip` move into the library; `write_stdout` stays.
- One entry point. `isomer::judgement::judge(old: &Path, new: &Path,
  &Options) -> Result<Judgement>`. A `Judgement` carries the verdict at both
  stages — `deterministic` is the rubric alone, `severity` is after the
  interpreter — plus the gate, the interpretation, and the JSON envelope.
  The envelope stays pre-serialized as a `RawValue`, so postdoc embeds it in
  a result without a parse or a re-encode.
- `Severity::band(self) -> scan::Classification`, the inverse of
  `Risk::model_severity`, so the mapping into scan's bands is isomer's.
- `pub(crate)` becomes `pub` where reached from those entry points;
  `missing_docs` is already a warning and gets satisfied on the way.
- `terminal`, `markdown`, `sarif`, `ci`, and `fs` stay binary-side.

## Layout

```
postdoc/
  Cargo.toml            bin `postdoc`, lib `postdoc`; deps: scan, isomer,
                        cleave (default-features = false), axum, tokio,
                        reqwest, serde, serde_json, anyhow, clap, tracing
  src/main.rs           clap: serve | worker
  src/lib.rs
  src/report.rs         wire types: Result, Verdict, Judge<R>, Status
  src/judge/mod.rs      run all judges for one Subject; combine()
  src/judge/cleave.rs
  src/judge/ml.rs
  src/judge/llm.rs      shared LLM breaker and budget live here
  src/judge/diff.rs     baseline bytes → temp pair → isomer::judge
  src/judge/diff_llm.rs
  src/baseline.rs       hopper baseline client (claim field or /api/baseline)
  src/server/mod.rs     router, state, config
  src/server/lookup.rs  /v1/lookup
  src/server/analyze.rs /v1/analyze
  src/server/ops.rs     /_/stats, /_/health
  src/worker.rs         the postdoc backend for scan::worker::run
  docs/DESIGN.md
  docs/JSON.md          field-by-field reference for the result object
```

Lints mirror scan's `[workspace.lints]` plus isomer's `unsafe_code = "deny"`.
No file over 2000 lines.

## Phases

1. **Libraries.** Isomer library split and `Severity::band`. Scan:
   `Analyzer::scan(Subject)`, the trait floor as a pure function, the worker
   backend, `Deserialize` on the wire types, the small server modules made
   public. Both CLIs and scan's daemon modes stay byte-identical, checked by
   their existing tests.

   - **done** — postdoc's wire types (`src/report.rs`) and the fold
     (`src/combine.rs`), with no dependency on an analysis stack, so the
     contract is reviewable and testable on its own.
   - **done** — scan's trait floor as the pure `engine::trait_floor`,
     returning a `FloorDecision` (class, confidence, level, arm, counts)
     instead of mutating in place. `apply_trait_floor` is now a thin wrapper
     over it, so scan's own paths and all 21 existing floor tests are
     unchanged; 5 tests were added, including one pinning the two forms
     against drift.
   - **done** — the isomer library split. `src/lib.rs` owns the modules and
     the shared types; `src/main.rs` keeps clap and is a thin verb dispatch.
     `Options` replaces `&Cli` everywhere in library code, with a `Default`
     pinned against clap's own defaults by a test. `isomer::judgement::judge`
     is the entry point postdoc calls: two paths in, a verdict at both stages
     plus the JSON envelope out. `Severity::band` maps isomer's five grades
     into scan's three, tested as the inverse of `Risk::model_severity`.
     Public surface is 40 items; everything else stayed `pub(crate)`.
   - **done** — postdoc links all three engines. The engine seam is
     `src/engine.rs`, the only module naming a foreign type: scan's
     `Classification` and firing level into postdoc's, and isomer's grade
     through its own `band`. A test there asserts the two engines agree on a
     shared band, which also fails to *compile* if scan is ever linked twice.
   - **done** — the concrete result (`src/result.rs`): the full report, a
     golden test pinning field order, a round trip, and the `full=1` strip.
   - **done** — scan reports the model and the floor apart. `ScanResult`
     gained `model` (what the model alone reached, root and members, before
     the floor) and `floor` (the gravest firing anywhere). Neither is
     serialized, so the envelope is byte-identical — scan's two
     serialization-parity tests prove it. Recovering the model's own reading
     is cheap because the floor fires only on a model-Benign decision: on a
     file it raised, the model said benign, and everywhere else the stored
     decision is already the model's. This is what lets the `cleave` and `ml`
     judges be two opinions rather than postdoc re-deriving one from the
     other. 6 tests added; scan's suite is 746 and still green.
   - **done** — the judges (`src/judge.rs`): each engine's result unpacked
     into the judge that shows its pieces, the two outcomes folded, the
     verdict written. An example runs it end to end against real bundles;
     `docs/example-report.json` is its output.
   - **done** — scan's worker startup resolves through `worker::Startup`.
     The model bundle, operating point, slot count, memory ceiling and
     trait-validation gate were decided in scan's CLI `main.rs`, private, so
     no second binary could start a worker the same way. They now live beside
     the worker they configure, and `atomscan`'s own command line is
     unchanged — same flags, same `--help`, 746 tests still green.
   - not started — `Analyzer::scan(Subject)`, the server modules made public.

   A third thing changed while building: **`duration_ms` on a judge is
   optional**. Only the interpreter is separately timed. Cleave's analysis and
   the model's inference happen in one pass through scan, and isomer's
   interpreter runs inside its judgement, so an invented share of a combined
   measurement would read like a real one to whoever is chasing a slow
   analysis. Splitting those timings properly is scan-side work, not a
   postdoc guess.

   Two things changed while building. **The baseline moved to the top level**
   of the result: it describes what was analyzed, like `sha256` beside it,
   not what an engine concluded, and no judge then needs a field its siblings
   lack. **Every engine's output rides as pre-serialized JSON** rather than a
   named type — postdoc never reads inside one, so naming five foreign types
   would buy nothing and would tie the wire contract to five crates'
   internals. That also made `Judge`'s deserializer hand-written: serde's
   internally-tagged representation buffers a value before dispatching on its
   tag, and a buffered value can no longer be borrowed by a pre-serialized
   payload. Parsing through a flat intermediate fixed it and moved the
   envelope's invariants to the boundary, where a malformed document is
   refused rather than half-accepted.

   A deviation from the plan above: isomer has one entry point, not two. The
   interpreter runs inside `judge` when `Options::llm` names an endpoint,
   because separating it would mean unpicking `Analysis::finish`. postdoc
   still reports the two judges apart, because a judgement carries the
   verdict both before and after the interpreter.
2. **Worker.** `postdoc worker` running scan's loop and posting the legacy
   `{ml, llm, raw}` body. Wire output is unchanged and hopper needs no
   change. This is the cutover-safe point: postdoc can take claims beside
   scan workers with no consumer noticing.

   - **done** — the binary exists and accepts the argv hopper supervises a
     local worker with, so the cutover is a binary name and nothing else.
     A test pins that argv.
   - The plan called for a backend trait here. There is nothing yet for one
     to abstract: phase 2 posts the legacy body, which is scan's own
     behaviour, so a trait would have one implementation and no second
     caller. It arrives in phase 3, where the posted body actually differs.
3. **Judges and the worker's new body.** The worker posts the result object;
   hopper accepts it and adds `verdict` and `judges`. No server involved, so
   nothing beamline calls changes yet.
4. **The server moves.** Scan's server modules come over intact and `/v1`
   answers byte-for-byte as it does now, proved by replaying recorded
   requests against both binaries. This is the step that lets a deployment
   swap `atomscan serve` for `postdoc serve`.

   Taken in two parts, because the second is only worth doing once the first
   proves the drop-in works:

   - **done** — `postdoc serve` exists and runs scan's server. Startup
     resolution moved to `server::Startup` beside `worker::Startup`: the
     listen address, body limit, memory ceiling, allowed directories, CIDR
     list, bearer token, model bundle, operating point and idle-worker slots
     were all settled in scan's CLI, private, so no second binary could serve
     the same way. `atomscan serve`'s own command line is unchanged.
   - **done** — the startup rule refresh is `scan::refresh_rules_at_startup`,
     shared by both binaries and both modes. It is also what creates a
     `--traits-dir` on a fresh deploy; without it a server starts, reports
     healthy, and fails every analysis.
   - in progress — scan's 38 global flags become a public `clap::Args` that
     both binaries flatten. Until then `postdoc serve` accepts only its own
     14 flags, so an operator passing `--llm` or `--fetch` gets different
     behaviour from `atomscan serve` and gets no warning about it. That is
     the gap between "runs the same server" and "is a drop-in".
   - not started — moving the server's source out of scan. Nothing needs it
     until `/v2`, and while both binaries call one implementation there is
     nothing to drift.
5. **`/v2`.** `/v2/lookup` and `/v2/analyze` answer with the result object.
   Beamline adopts it when it wants the judges; until then it keeps reading
   `/v1` and nothing has to move.
6. **Diff.** Hopper: `purl` and `baseline` on claims, `/api/baseline`.
   postdoc: `diff` and `diff_llm` judges.

Retiring scan's serve and worker modes is separate work, after postdoc has
rolled out.

## Testing

- `combine()` is a table test over every (scan outcome, isomer outcome)
  band pair, including the no-baseline and skipped-judge cases, and checks
  `decided_by` and the level placement for each.
- A golden test pins the result object's field order and the
  every-key-present rule, the way isomer's `envelope_shape_is_stable` does.
- `/v1` compatibility is proved by replay, not by reading: the same recorded
  requests go to `atomscan serve` and `postdoc serve`, and the responses are
  compared body and header. Reading two implementations and agreeing they
  look equivalent is how the small differences survive.
- Worker and server run against scan's existing mock hopper binary, which
  gains the `baseline` claim field and `/api/baseline`.
- Phase 2 is verified by diffing `POST /api/result` bodies from `atomscan
  worker` and `postdoc worker` over the same samples.
