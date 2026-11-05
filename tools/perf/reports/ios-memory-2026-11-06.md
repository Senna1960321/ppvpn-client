# iOS-shaped memory validation, 2026-11-06

Both strict Linux matrices now complete successfully: **2,000/2,000 valid DNS replies** from the local fixture and from the public path. The deterministic DNS lifetime regression also changes from **2/3 to 3/3** replies. Whole-process Linux RSS remains above 35 MiB, and this is not Apple/device acceptance.

## Sources and build

- Client harness: `7af452d`, with the existing 128-cap and strict response validation checks.
- Sail: [`3fa1afcd10f7c70fb0b4a699528b26f37fa7045a`](https://github.com/Senna1960321/sail/commit/3fa1afcd10f7c70fb0b4a699528b26f37fa7045a), an explicit experimental composition of upstream `ea07e9c6`, [DoT retry PR #35](https://github.com/peakpassvpn/sail/pull/35), and [DNS lifetime PR #36](https://github.com/peakpassvpn/sail/pull/36). The two fixes remain separate PRs.
- Linux ARM64, kernel `6.8.0-147-generic`, two Tokio workers, Sail runtime mobile tuning and `inbound.max_connections=128`.
- Cargo `--profile mobile --locked --features ppvpn-core/lab`, matching the build profile used by the mobile tunnel host. The earlier report used Cargo `--release`; its phrase “mobile profile” referred to runtime tuning. These runs also change source code and DNS conditions, so they do not isolate the memory effect of LTO.
- Measurement binary SHA-256: `1dffac042c5bfadfffb5e15fa44e7928f694833a2961954ff548a20f07a7f6c7`.
- Both matrices ran without competing compilers or benchmarks. Each used the full workload: 30-second idle, 100/250/500 held streams, 20-second downloads, node switches, 2,000 distinct DNS queries at 400/s, rule-set refresh, settling and warm restart.

The production Sail dependency remains its existing release tag. The [separate dependency experiment patch](ios-memory-sail-3fa1afcd.patch) pins the measured Sail composition and dependency graph; it is not a release dependency upgrade.

## DNS defect and the repeatable load

NAT cleanup previously treated the first DNS answer as permission to expire its entire UDP session. With pipelined queries, that could abort the downlink while another query still awaited a response. A three-query local TLS test through the real TUN and AnyTLS path reproduced this without public resolver uncertainty: one slow answer was generated at about 10.26 seconds but never delivered. The fixed binary delivers all three answers, including that slow one across the cleanup tick.

PR #36 tracks admitted queries through the resolver, answer queue and delivery to the client. Prompt cleanup resumes when the work drains; the configured idle deadline remains a hard bound. Regression tests cover delayed replies, a full client channel, failed admissions and eventual cleanup.

For repeatable memory measurements, the harness now defaults to a local verified DoT responder with a 100 ms delay. Only the three configured resolver IPs on TCP port 853 are redirected inside the fake node. The TUN, Sail DNS client, proxy path and TLS verification remain active. A separate helper exports the native trust roots plus the temporary fixture certificate before the measured process starts; system trust is unchanged. This load completed on **one TLS connection**, and the fixture recorded all 2,000 queries and replies.

`--dns-source public` keeps the public path explicit, with the same 95% valid-response threshold. Its full matrix also returned 2,000/2,000. This is a successful controlled rerun alongside a deterministic defect reproduction, not a guarantee about every resolver or network condition. SERVFAIL, REFUSED, truncation, duplicates and unrelated questions still cannot make the check pass. A passing local load alone does not establish public-path correctness.

The `node_sessions_peak` field counts all established AnyTLS sessions, including idle pooled sessions left by earlier loads. It is not a count of new DoT connections. `fixture_connections` measures the local responder's TLS connections separately.

## Memory and admission results

Both commands exited zero. Each 100-stream load completed 100 echoes; the 250/500 overloads each completed exactly 128, with no stalls. Refused excess streams remain visible as failed/refused rows in the raw report and are accepted only under the explicit iOS cap rule.

All values below are peak MiB. Linux `RssAnon` excludes resident file-backed pages; whole-process RSS is a separate sampled measurement.

| Scenario | Local RssAnon | Local RSS | Public RssAnon | Public RSS |
| --- | ---: | ---: | ---: | ---: |
| Start | 7.46 | 28.97 | 7.24 | 28.73 |
| Idle | 7.49 | 29.06 | 7.27 | 28.82 |
| 100 connections | 18.76 | 42.02 | 19.92 | 43.16 |
| 250 connections | 19.24 | 42.49 | 21.39 | 44.63 |
| 500 connections | 22.34 | 45.59 | 21.80 | 45.04 |
| AnyTLS download | 29.92 | 53.18 | 29.50 | 52.74 |
| Shadowsocks download | 29.92 | 53.24 | 29.50 | 52.80 |
| Node switches | 29.92 | 53.24 | 29.50 | 52.80 |
| DNS burst | 29.98 | 53.73 | 29.72 | 53.52 |
| Rule-set refresh | 31.60 | 54.62 | 31.12 | 54.34 |
| Settled | 19.86 | 43.73 | 20.54 | 44.46 |
| Warm restart | 23.01 | 47.04 | 24.15 | 48.41 |

Maximum RssAnon / whole-process RSS were **31.60 / 54.62 MiB** locally and **31.12 / 54.34 MiB** on the public path, all at rule-set refresh. Maximum counted heap was 17.22 / 17.05 MiB respectively. Heap and resident peaks need not occur simultaneously; their difference does not directly identify a component's cost.

The 35 MiB whole-process budget is still unmet. Neither Linux metric establishes iOS `phys_footprint` acceptance, and this harness excludes the Swift host. Rule sets are fetched and hashed at runtime; these results are snapshots rather than a controlled comparison with yesterday's data.

## Validation and reproduction

Harness checks: 31 Python tests, Go fake-node/load-generator race tests, and three Rust trust-helper tests pass. Actual macOS helper checks preserve native roots and explicit certificate overrides, reject empty/malformed input, and do not alter system trust. The source-counter regression fails against the previous harness and passes with this change. The CI memory jobs now build the mobile profile and run the Go fixture tests. Changes to nested `ne-mem` helpers also trigger the memory gate; a real-selector regression checks that coverage.

PR #36 validation includes three focused regressions, 131 related unit tests plus one ignored test, three DNS integration tests, and Clippy with warnings denied for the tested minimal feature set. These are targeted checks, not a claim that the entire feature matrix ran locally. CI status is tracked on that PR.

From a clean checkout of this PR, explicitly opt into the candidate:

```sh
git apply --unidiff-zero tools/perf/reports/ios-memory-sail-3fa1afcd.patch
cargo build --profile mobile --locked -p ppvpn-core-lab --bin ne-mem --features ppvpn-core/lab
mkdir -p build/perf
(cd test && CGO_ENABLED=0 go build -trimpath -o ../build/perf/fakenode ./perf/fakenode)
(cd test && CGO_ENABLED=0 go build -trimpath -o ../build/perf/loadgen ./perf/loadgen)
project=$(pwd)
results=$(mktemp -d)
sudo sh -c 'cd "$2"; ulimit -n 16384; exec python3 "$1/tools/perf/ne_mem.py" \
  --ne-mem "$1/target/mobile/ne-mem" --fakenode "$1/build/perf/fakenode" \
  --loadgen "$1/build/perf/loadgen" --workers 2 --variant ios --no-default \
  --dns-source local --out ios-local.json' sh "$project" "$results"
```

Run the same command with `--dns-source public --out ios-public.json` separately. Use an isolated Linux environment with TUN access; the harness temporarily routes TEST-NET-3 to the tunnel. Keep generated files outside the repository. The pinned patch applies cleanly and resolves with `cargo metadata --locked`.

## Remaining acceptance

The PR remains a draft: whole-process Linux RSS exceeds the budget, Apple/device footprints have not been measured for this candidate, and production adoption still needs a Sail release tag containing the fixes. No merge, release or physical-device acceptance is claimed. The public DNS failure reported on November 5 is resolved in this rerun; it is no longer listed as an observed failing load for this candidate.
