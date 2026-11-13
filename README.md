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

## postdoc grades nothing

Every severity in a result belongs to the engine that produced it.

| Juror       | Engine | Decides |
| ----------- | ------ | ------- |
| `ml`        | the [azoth](https://github.com/atomdrift-project/azoth) model, read through [scan](https://github.com/atomdrift-project/scan) | the tightest false-positive budget at which the artifact grades hostile |
| `floor`     | [cleave](https://github.com/atomdrift-project/cleave)'s traits, graded by scan's trait floor | whether severe traits corroborate each other across enough behaviour families |
| `llm`       | scan's per-file interpreter | whether the code reads as benign, suspicious or hostile |
| `drift_heuristic`  | isomer, over this artifact and an earlier release | what this release introduced |
| `drift_llm` | isomer's per-diff interpreter | whether that change reads as an attack |

Third-party scanners report beside the jurors and never move the verdict:
the worker and the server also run ClamAV, malcontent and GuardDog on every
artifact, by default, and send what each found under `scanners`. Each is
checked at startup; `make scanners` installs them, and `--scanners` picks a
subset or `none`.

postdoc owns two things: the order the engines are asked in, and keeping the
worse of the two finished outcomes — scan's and isomer's. It holds no
thresholds, no trait lists and no prompts. A verdict here can be traced to an
engine and a build, and re-derived by running that engine again.

That constraint is the point. A coordinator that quietly re-weighed its
inputs would be a sixth juror nobody could audit.

## The result

postdoc sends scan's result: the version 8 body scan uploads to hopper,
JSON lines with a header, one line per file (the artifact first, then up to
256 of its members), and one line per report. Every file line carries its
verdict and one object per juror that spoke; the artifact's line also
carries what it points at (`refs`).

When there was an earlier release to compare against, postdoc adds isomer's
two opinions to the artifact's line as `drift_heuristic` and `drift_llm`, and the
verdict becomes the worse of scan's and isomer's, with `by` naming the juror
that decided it:

```json
{"sha256":"…","size":48211,"type":"npm",
 "verdict":{"outcome":"ok","class":"hostile","fires_at":25,"by":"drift_heuristic"},
 "ml":{"class":"benign","fires_at":-1,"prob":0.05,"model":"v20.20"},
 "llm":{"class":"benign","conf":0.8,"model":"…","reason":"…"},
 "drift_heuristic":{"class":"hostile","fires_at":25,"base":"9e1c…","engine":"0.5.0"},
 "drift_llm":{"class":"hostile","model":"…","reason":"postinstall now fetches a remote script","base":"9e1c…"},
 "report":"9f…"}
```

`fires_at` is the tightest false-positive budget, per 100 million benign
files, at which the artifact grades hostile. Lower is worse; `-1` means it
fires at none. It is *measured*, and turning it into an allow or a block
against a caller's own budget happens when a lookup is answered, never here.

The worker posts this body to hopper. `postdoc serve` answers
`/v1/analyze?full=1` with it, without the worker name, and answers every
other `/v1` request with scan's decision.

## Modes

```
postdoc worker --url https://hopper.example --name nuc
```

**worker** claims artifacts from hopper, judges them, and posts results back.
It accepts the same arguments `atomscan worker` does, so hopper's supervised
local worker is a binary-name change and nothing else.

**serve** implements Scan's existing `/v1` HTTP API with the same routes,
request forms, authentication, and response shapes. Beamline can keep referring
to these as Scan servers during the mixed-fleet transition; point new and
replacement hosts at `postdoc serve` without changing `SCAN_URL` consumers.

The worker and server implementations now live here. The server keeps Scan's
`/v1` contract, and the worker posts the same version 8 body
`atomscan --hopper` does.

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

[docs/DESIGN.md](docs/DESIGN.md) is the plan of record: the jurors, how the
fold works, how a baseline is chosen, and the changes it needs in hopper,
scan and isomer. [docs/JSON.md](docs/JSON.md) is what postdoc adds to scan's
version 8 body.

Licensed Apache-2.0.
