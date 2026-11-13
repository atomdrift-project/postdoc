# What postdoc sends

postdoc sends scan's result, not one of its own. `postdoc worker` posts it
to hopper's `/v1/work/result`, and `postdoc serve` answers `/v1/analyze?full=1`
with it. It is the version 8 body specified in scan's `JSON_PLAN.md` and
summarized in scan's `docs/JSON.md`: JSON lines (`application/x-ndjson`,
zstd-compressed on the way to hopper), a header, one line per file, then one
line per report.

This page covers only what postdoc adds to that body. Everything else is
scan's, built by scan's upload document, and postdoc writes no field outside
what is listed here.

## The header

```json
{"v":8,"scan":{"engine":"3.0.0","ruleset":"76e61","level":25,"worker":"nuc","at":"2026-11-12T15:00:00Z","ms":4120},"files":3}
```

| Field | Worker post | `?full=1` |
| --- | --- | --- |
| `scan.worker` | the worker's `--name`; hopper refuses a body without one | absent |
| `scan.ms` | how long the analysis took, in milliseconds | absent |

## The drift jurors

When hopper named an earlier release, isomer judges the difference, and its
two opinions join scan's `ml`, `floor` and `llm` on the artifact's line (the
first file line) as `drift_heuristic` and `drift_llm`. Without a baseline neither is
present, and hopper removes any it held for that file.

```json
"drift_heuristic":{"class":"hostile","fires_at":25,"base":"9e1c…","engine":"0.5.0"},
"drift_llm":{"class":"suspicious","model":"qwen3-…","reason":"postinstall now fetches a remote script","base":"9e1c…"}
```

| Field | Juror | Meaning |
| --- | --- | --- |
| `class` | both | `benign`, `suspicious` or `hostile`: isomer's band |
| `base` | both | sha256 of the release compared against |
| `fires_at` | `drift_heuristic` | where the band sits on the level grid. Nothing about a change was calibrated against a benign corpus, so it is placed the way scan places its own interpreted verdicts |
| `engine` | `drift_heuristic` | isomer's version |
| `model`, `reason` | `drift_llm` | the interpreter's model, and its phrase, at most 400 bytes |

Each object is at most 900 bytes and carries no NULs, as hopper requires of
every juror.

## The verdict

The artifact's `verdict` is the worse of scan's and isomer's finished
outcomes, where isomer's has its own interpreter already applied: the more
severe band wins, and within one band the tighter level does
(`combine::verdict`). When isomer's wins, the verdict takes its band and
level, and `by` names the juror that decided it: `drift_llm` when the
interpreter moved isomer to that band, and `drift_heuristic` otherwise. When scan's
wins it is unchanged, including its `by`, `in` and `reason`.

Members are never touched: their lines are scan's.

## Other scanners

`postdoc worker` and `postdoc serve` run ClamAV (through clamd), malcontent
and GuardDog on every artifact they analyze, by default, and send what each
said on the artifact's line under `scanners`, keyed by name (scan's
`JSON_PLAN.md`, "Other scanners"). A scanner never moves the verdict, and
hopper keeps each one's finding as its own verdict row.

Each scanner is found when postdoc starts (clamd's socket from its
`clamd.conf`, `mal` and `guarddog` on `PATH`) and checked: clamd must answer,
`mal` must give its version, and GuardDog must scan an empty npm package,
which proves the sandbox it insists on works. postdoc refuses to start
without a scanner it was asked for, and says which and why. `--scanners`
(`POSTDOC_SCANNERS`) picks which run — `clamav,malcontent,guarddog` by
default, or `none` — and `--clamav`, `--malcontent` and `--guarddog` name
where one is when discovery will not find it. `make scanners`, and every
deploy, install them.

```json
"scanners":{
  "clamav":{"class":"hostile","signature":"Eicar-Test-Signature","engine":"1.5.4","db":"28044"},
  "malcontent":{"class":"hostile","risk":"critical","files":1,"behaviors":["3P/elastic/multi_eicar"],"engine":"1.28.1","ruleset":"1.28.1"},
  "guarddog":{"class":"suspicious","risk":5.5,"label":"suspicious","threats":["shady-links"],"engine":"3.0.1","ruleset":"3.0.1"}}
```

| Scanner | `class` | Its own fields |
| --- | --- | --- |
| `clamav` | `hostile` for a signature, `suspicious` for ClamAV's hedges (`PUA.*`, `Heuristics.*`), `benign` for none | `signature`; `engine` and `db`, from clamd's `VERSION` at scan time |
| `malcontent` | its worst file's risk: `critical` is hostile, `high` suspicious, anything lower benign | `risk`, how many `files` reached it, and up to five `behaviors` at that risk; `engine`, and `ruleset` (the same version: its rules ship inside it) |
| `guarddog` | its risk score (0–10): 7 and up hostile, 5 and up suspicious, else benign | `risk`, GuardDog's `label` for it, and up to five `threats` (the threat rules that fired); `engine` and `ruleset` as for malcontent |

GuardDog reads only the packages it knows: npm packages, Python wheels,
sdists and eggs, Go modules and Ruby gems. For anything else it does not run
and sends nothing. It reads an unpacked tree, so the package is unpacked into
a temporary directory first (at most 20,000 entries and 512 MiB, links and
anything that would leave the directory skipped), and without `--metadata`
only its source rules run, offline. A clean score from rules that failed is
sent as an error, not as benign.

A scanner that ran always reports; one that did not run is left out, which
hopper reads as no statement, so a host without clamd (or `atomscan
--hopper`) never erases ClamAV's finding. One that failed sends `outcome`
and `error` instead of a class, which leaves its last finding standing: `timeout` (clamd
silent for 300 s, malcontent or GuardDog over 600 s), `unsupported` (over clamd's
`StreamMaxLength`, or a ClamAV scan limit reached), or `error`.

ClamAV is streamed the bytes (`INSTREAM`), so clamd needs no access to the
worker's files. malcontent reads a path, so a sample held in memory is written
to a temporary directory under its own name first; it runs with one job,
since the worker already runs one analysis per slot.

## Failures

A failed analysis has no version 8 form hopper can classify yet, so the
worker posts it as version 7's error object (`application/json`):

```json
{"sha256":"ab12…","worker":"nuc","error":"analysis exceeded its deadline","duration_ms":0}
```

Hopper reads `error` to decide whether the file is missing, timed out,
unsupported or failed.
