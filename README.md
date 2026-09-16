# postdoc

**Every engine gets a say, and the result says who said what.**

postdoc is the process between [hopper](https://github.com/atomdrift-project/hopper)
and the analysis engines. It claims artifacts from the corpus, asks each engine
what it makes of one, and reports the answers together with the verdict they
fold into. It replaces `atomscan serve` and `atomscan worker`.

It adds one thing neither had. When an artifact names a package, hopper can
point at an earlier release of that package it already holds, and
[isomer](https://github.com/atomdrift-project/isomer) judges the difference. A
release that looks clean on its own can still have *added* something, and that
is the shape most supply-chain compromises take.

> NOTE: EARLY BUILD — the wire format is not yet stable.

## postdoc grades nothing

Every severity in a result belongs to the engine that produced it.

| Judge      | Engine | Decides |
| ---------- | ------ | ------- |
| `cleave`   | [cleave](https://github.com/atomdrift-project/cleave)'s traits, graded by scan's trait floor | whether severe traits corroborate each other across enough behaviour families |
| `ml`       | the [azoth](https://github.com/atomdrift-project/azoth) model, read through [scan](https://github.com/atomdrift-project/scan) | the tightest false-positive budget at which the artifact grades hostile |
| `llm`      | scan's per-file interpreter | whether the code reads as benign, suspicious or hostile |
| `diff`     | isomer, over this artifact and an earlier release | what this release introduced |
| `diff_llm` | isomer's per-diff interpreter | whether that change reads as an attack |

postdoc owns two things: the order the engines are asked in, and keeping the
worse of the two finished outcomes — scan's and isomer's. It holds no
thresholds, no trait lists and no prompts. A verdict here can be traced to an
engine and a build, and re-derived by running that engine again.

That constraint is the point. A coordinator that quietly re-weighed its
inputs would be a sixth judge nobody could audit.

## The result

One artifact produces one verdict and, beside it, one judge object per engine.

```json
{
  "v": "1",
  "sha256": "…",
  "purl": "pkg:npm/left-pad@1.3.1",
  "verdict": {
    "severity": "hostile",
    "fires_at": 25,
    "reason": "postinstall fetches and executes a remote script",
    "findings": [{ "id": "…", "crit": 5, "desc": "…" }],
    "decided_by": ["ml", "diff"],
    "engines": { "postdoc": "0.1.0", "scan": "2.12.0", "…": "…" },
    "analyzed_at": "2026-09-15T12:00:00Z",
    "duration_ms": 4120
  },
  "cleave":   { "status": "ok", "severity": "benign",  "fires_at": null, "…": "…" },
  "ml":       { "status": "ok", "severity": "hostile", "fires_at": 25,   "…": "…" },
  "llm":      { "status": "ok", "severity": "hostile", "confidence": 0.95, "…": "…" },
  "diff":     { "status": "ok", "severity": "hostile", "fires_at": 25,   "…": "…" },
  "diff_llm": { "status": "skipped", "reason": "no_baseline" }
}
```

Every judge key is present in every result. An engine that was not asked says
so and why; one that failed says that instead, with how long it ran before it
did. A consumer writes one code path.

Each judge carries its own engine's native output under `raw` — the cleave
report, the model's route scores, isomer's differential. The evidence sits
with the opinion it supports rather than in a shared pile, so a judge object
is readable on its own. `raw` is dropped unless the caller asks for it with
`full=1`.

`fires_at` is the tightest false-positive budget, per 100 million benign
files, at which the artifact grades hostile. Lower is worse; `-1` means it
fires at none. It is *measured*, and turning it into an allow or a block
against a caller's own budget happens when a lookup is answered, never here.

## Modes

```
postdoc worker --url https://hopper.example --name nuc
postdoc serve  --addr 0.0.0.0:8080
```

**worker** claims artifacts from hopper, judges them, and posts results back.
It accepts the same arguments `atomscan worker` does, so hopper's supervised
local worker is a binary-name change and nothing else.

**serve** answers [beamline](https://github.com/atomdrift-project/beamline),
its only caller: `GET /v1/lookup` for what is already known, `POST
/v1/analyze` to spend an analysis slot, plus `/_/stats` and `/_/health`.

There is no one-shot CLI. `atomscan` and `isomer` are the tools for that.

## Build

```sh
make build     # debug
make test      # the suite
make lint      # rustfmt --check, then clippy with warnings denied
make release
```

`make install-precommit` gates commits on both.

## Design

[docs/DESIGN.md](docs/DESIGN.md) is the plan of record: the judges, how the
fold works, how a baseline is chosen, and the changes it needs in hopper,
scan and isomer. [docs/JSON.md](docs/JSON.md) is the field-by-field reference
for the result object.

Licensed Apache-2.0.
