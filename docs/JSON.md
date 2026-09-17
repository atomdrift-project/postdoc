# The postdoc result object

One analyzed artifact produces one of these. It is what `postdoc worker`
posts to hopper, and what `postdoc serve` answers on `/v2`.

It does **not** appear on `/v1`. That is scan's contract and postdoc inherits
it unchanged, so `/v1/analyze?full=1` keeps returning `{ml, llm, raw}` and
`/v1/lookup` keeps answering the decision shape it answers today. This object
is a different shape rather than an extension of that one, which is what
makes it a new version instead of a new field.

Schema version `"1"` — the object's own, independent of the route's. This
document is the contract; the types are in `src/report.rs`.

## Top level

| Field      | Type            | Notes |
| ---------- | --------------- | ----- |
| `v`        | string          | Schema version. `"1"`. |
| `sha256`   | string          | The analyzed bytes. 64 lowercase hex. |
| `purl`     | string \| null  | The package coordinate the artifact was fetched as, when one is known. |
| `baseline` | object \| null  | The earlier release this one was compared against. Below. |
| `verdict`  | object          | What postdoc concluded. Below. |
| `cleave`   | judge           | Cleave's traits, graded by scan's trait floor. |
| `ml`       | judge           | The azoth model, read through scan. |
| `llm`      | judge           | Scan's per-file interpreter. |
| `diff`     | judge           | Isomer, against an earlier release. |
| `diff_llm` | judge           | Isomer's per-diff interpreter. |

The five judge keys are **always present**. A consumer never has to test for
one; it reads `status` and branches there.

## `verdict`

| Field            | Type           | Notes |
| ---------------- | -------------- | ----- |
| `severity`       | string         | `benign`, `suspicious` or `hostile`. |
| `fires_at`       | int \| null    | See [Levels](#levels). |
| `reason`         | string         | One sentence from whichever interpreter spoke. **Omitted** when none did. |
| `findings`       | array          | The worst findings, worst first. `[]` when there are none. |
| `decided_by`     | array<string>  | The judges whose own band equals `severity`. |
| `engines`        | object         | The builds behind this result. |
| `analyzed_at`    | string         | RFC 3339, UTC. |
| `duration_ms`    | int            | The whole analysis, judges included. |

There is deliberately **no `decision` field**. Whether an artifact is allowed
or blocked depends on the asking caller's false-positive budget, so it is
computed when a lookup is answered and never stored. A stored verdict that
already chose for the caller would be wrong for every caller with a different
budget.

`decided_by` answers "who decided this", so a reader can tell a model verdict
from a trait-floor verdict from a differential one without re-deriving the
fold. Judges that did not report are never credited.

### `engines`

Every field is a string: `postdoc`, `scan`, `cleave`, `isomer`, `traits` (the
traits bundle revision), `model` (the model bundle identifier). Recorded per
result rather than per deployment, because a corpus holds verdicts from many
builds at once and re-analysis decisions turn on which one produced a row.

### `findings`

| Field  | Type   | Notes |
| ------ | ------ | ----- |
| `id`   | string | Stable trait identifier, e.g. `objectives/execution/shell/bash`. |
| `crit` | int    | 3 notable, 4 suspicious, 5 hostile. |
| `file` | string | Member file within an archive. Omitted when absent. |
| `pkg`  | string | The package the member belongs to. Omitted when absent. |
| `desc` | string | One line. Omitted when absent. |
| `off`  | int    | Byte offset of the match. Omitted when absent. |
| `line` | int    | Line number, in text. Omitted when absent. |

The same shape scan's `/v1/lookup` answers with, so a reader that already
parses that parses this.

## A judge

Three shapes, discriminated by `status`.

**`"ok"`** — the engine ran and reported.

| Field         | Type          | Notes |
| ------------- | ------------- | ----- |
| `status`      | string        | `"ok"`. |
| `severity`    | string        | `benign`, `suspicious` or `hostile`. |
| `fires_at`    | int \| null   | Always present, `null` when the engine measures no level. |
| `confidence`  | float         | `0..1`. **Omitted** where the engine reports none. |
| `duration_ms` | int           | How long this engine took. **Omitted** where it is not separately measured — see below. |
| `version`     | string        | The engine build, model or ruleset behind it. |
| `raw`         | object        | The engine's native output. **Omitted** unless asked for. |

`duration_ms` is omitted rather than guessed. Some engines share a call:
cleave's analysis and the model's inference happen inside one pass through
scan and are not split, and isomer's interpreter runs inside its judgement.
An invented share of a combined measurement would read like a real one to
whoever is chasing a slow analysis.

**`"skipped"`** — the engine was not asked. Exactly two fields, `status` and
`reason`. Nothing ran, so there is nothing else to report.

| `reason`          | Meaning |
| ----------------- | ------- |
| `no_baseline`     | No earlier release of this package to compare against. |
| `not_configured`  | The engine is not configured in this deployment. |
| `not_admitted`    | The engine's own admission gate declined this artifact. |
| `unavailable`     | Configured, but unreachable or out of capacity. |

A closed set, because consumers branch on it.

**`"error"`** — the engine was asked and failed. `status`, a free-text
`reason`, and `duration_ms`. Failures are open-ended in a way that "we did
not ask" is not, so the reason is a string rather than an enum. The duration
is there because an engine that fails slowly and one that fails immediately
are different operational problems.

### `raw`

Each judge carries its own engine's output, verbatim:

| Judge      | `raw` is |
| ---------- | -------- |
| `cleave`   | the cleave `CompactReport` |
| `ml`       | scan's `ml` section: probability, route scores, skipped routes, per-member verdicts |
| `llm`      | scan's interpretation: grade, rationale, the ML verdict it was blended with |
| `diff`     | isomer's report: behaviour mass, trait shift, risk delta, frameworks, evidence |
| `diff_llm` | isomer's interpretation |

Evidence sits with the opinion it supports, so a judge object is readable on
its own and no top-level key has ambiguous provenance.

`raw` is dropped unless the caller asks for it. Posts to hopper carry
everything; when a result exceeds hopper's body cap, `cleave.raw` is dropped
first, as scan's worker does today.

## `baseline`

The earlier release the `diff` judge compared against, or `null` when there
was none — in which case `diff` and `diff_llm` are both skipped with reason
`no_baseline`.

| Field       | Type          | Notes |
| ----------- | ------------- | ----- |
| `sha256`    | string        | The bytes compared against. |
| `purl`      | string        | That release's coordinate. Omitted when unknown. |
| `version`   | string        | Its version string. Omitted when unknown. |
| `label`     | string        | Its corpus label, in hopper's vocabulary. Omitted when unknown. |
| `fires_at`  | int \| null   | The budget at which the baseline itself grades hostile. |

It sits at the top level rather than inside the `diff` judge because it
describes *what was analyzed*, like `sha256` and `purl` beside it. It is an
input postdoc chose, not an opinion any engine reported.

A baseline may itself be convicted. Isomer handles a hostile old side, and
`label` and `fires_at` are here so a reader can weigh the comparison without
a second lookup.

## Levels

`fires_at` is the tightest false-positive budget, in false positives per 100
million benign files, at which the artifact grades hostile.

| Value     | Meaning |
| --------- | ------- |
| `0..N`    | Fires at this budget. **Lower is worse.** |
| `-1`      | Fires at no calibrated budget. |
| `null`    | No level applies — the engine ran under manual thresholds. |

Scan calls this `lvl` and hopper calls it `fires_at`; all three encode
"never" as `-1`, so the fleet agrees on the wire.

It is *measured*, not chosen. The caller's own tolerance is a separate
number, and comparing the two is what produces an allow or a block. An
artifact firing at 25 is convicted by a caller who tolerates 25 or more false
positives per 100M, and allowed by one who tolerates fewer.

## How the verdict is folded

Two engines each produce one finished outcome:

- **scan's** is the azoth decision, raised by the trait floor, then blended
  with its interpreter under scan's own bound. The `cleave`, `ml` and `llm`
  judges show the pieces; scan folds them.
- **isomer's** is the differential grade with its own interpreter already
  applied. The `diff` and `diff_llm` judges show the pieces; isomer folds
  them. With no baseline there is no isomer outcome.

postdoc keeps the worse of the two. Between two outcomes in the same band it
keeps the tighter budget, and a measured budget beats no budget. It does not
re-weigh the pieces and adds no opinion of its own.

A result where no engine reported has a `null` `fires_at` and is *not* a
clean bill — it means nothing judged the artifact, which a caller turns into
an unanalyzed answer rather than an allow.
