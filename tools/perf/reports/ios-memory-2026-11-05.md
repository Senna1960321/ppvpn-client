# iOS-shaped memory validation, 2026-11-05

Historical results. See the [November 6 follow-up](ios-memory-2026-11-06.md) for the DNS lifetime fix and current strict matrix results.

This is a Linux diagnostic for [#388](https://github.com/peakpassvpn/ppvpn-client/issues/388), not iPhone acceptance. The release dependency stays on its existing tag. The candidate includes newer Sail changes that have not yet shipped in a release tag.

## Workload and provenance

- Client engine sources: `d7f07176b80ad8ec62ced19606e31eec04bf79f7`; unchanged through the later harness revision `ebfc59e74120ecd550800e5590319d3834b0a066`.
- Strict DNS result validation from that later revision, plus this PR's check that the iOS load really is capped at 128 completed streams.
- Upstream Sail candidate: `ea07e9c6e59e765fca5bdb32f4183e5a86561050`, including DoT pipelining and queued send-buffer accounting fixes. Its dependency patch is [included separately](ios-memory-sail-ea07e9c6.patch).
- Linux ARM64 VM, kernel `6.8.0-147-generic`, two Tokio workers. Cargo `--release` build with `--features ppvpn-core/lab`, Sail runtime mobile tuning and `inbound.max_connections=128`. This run did not use Cargo `--profile mobile`; subsequent validation corrects that build-profile mismatch. The lab feature is required: otherwise the variant environment settings are ignored.
- Full harness defaults: 30 seconds idle, 100/250/500 held streams, 20-second downloads, node switches, 2,000 distinct DNS questions at 400 queries/second, rule-set refresh and warm restart. The DNS acceptance threshold remains 95%; SERVFAIL, REFUSED, truncated and unrelated replies do not count.
- Rule sets are downloaded and verified against hashes obtained at run time. Their contents can change between runs; these measurements are snapshots, not a controlled historical speed comparison.

## Upstream candidate result

The required command exited nonzero. Transfers and node switches completed; DNS answered **1,337/2,000** valid queries. The 100-stream workload completed all 100 echoes. Both overload scenarios completed exactly 128 echoes, with the excess streams refused as expected.

| Scenario | Peak anonymous resident memory, MiB | Peak whole-process RSS, MiB |
| --- | ---: | ---: |
| Start | 7.78 | 31.89 |
| 100 connections | 18.70 | 44.38 |
| 250 connections | 21.27 | 47.13 |
| 500 connections | 22.28 | 48.14 |
| AnyTLS download | 29.71 | 55.64 |
| Shadowsocks download | 29.71 | 55.70 |
| Node switches | 29.71 | 55.70 |
| DNS burst | 29.89 | 56.31 |
| Rule-set refresh | **31.15** | **56.78** |

`peak_kib` on Linux is sampled `RssAnon`; it excludes resident file-backed pages. A value below 35 MiB in that column does **not** establish the whole-process RSS budget or the iOS extension's `phys_footprint` budget. Whole-process RSS exceeded 35 MiB. No physical iPhone or macOS footprint acceptance was performed.

Measurement binary SHA-256: `4084bdf4121dd6c9fd8f01d11b9c81e33612b83873522ffc1b7d0a0a33096869`.

## Candidate with the DoT retry fix

A second full strict matrix used the same workload and dependency versions, with the focused sequential-retry fix applied to Sail. No compiler or competing benchmark ran during the matrix. All transfer, switch and capped-connection workloads passed, but DNS still answered only **1,403/2,000**, so the required command again exited nonzero. Peak anonymous resident memory was **31.29 MiB** and peak whole-process RSS was **57.08 MiB**, both at rule-set refresh. These two single runs do not establish a statistically meaningful improvement in public DNS completion.

The measured production `dot.rs` SHA-256 is `8e9532c53f14a3eca02d57ac46e7edd130a281eacdc40562d007b89799d64625`; the release measurement binary SHA-256 is `8fe31d4b628d30bae6d4a621ae89d40c6148154b019bbc5e7cbb986c161099e1`. The local path dependency used for this experiment does not alter the release dependency in this PR.

## DNS controls

These are functional controls, not additional memory acceptance runs.

| Control | Valid replies / 2,000 | Observation |
| --- | ---: | --- |
| Public DoT, capped mobile | 1,228 | Received replies were NOERROR; median response latency 1.63 seconds. |
| Public DoT, uncapped mobile | 1,323 | Received replies were NOERROR; median response latency 2.15 seconds. |
| Public DoT, receiver counters recorded | 1,108 | `RcvbufErrors`, `InErrors` and `MemErrors` all increased by zero; the receiver buffer was 212,992 bytes. |
| Local DoT, 100 ms response delay | 2,000 | One TLS connection, at most 38 in-flight questions, all replies NXDOMAIN. |
| Local DoT, first TLS connection silent | 2,000 | Recovered through a second connection; median response latency 3.10 seconds. |

The local responder used the same client profile and proxy path, a test certificate trusted only by the measurement process, and loopback redirection in the fake node. It changed no system trust store. These controls establish working cold pipelining and fallback with a deterministic responder. They do not establish the cause of the missing replies on the public resolver path. The kernel counters do not support receiver-buffer overflow as that cause. The sampled node-session count includes idle AnyTLS sessions left by earlier loads and cannot be treated as the number of new DNS connections.

An independent review found a narrower DoT retry defect: concurrent sequential retries could each discard a healthy replacement connection. Its focused fix and regression tests are separate from attribution of the public-path shortfall.

## Reproducing the upstream candidate

From a clean checkout of this PR, apply the dependency experiment explicitly:

```sh
git apply --unidiff-zero tools/perf/reports/ios-memory-sail-ea07e9c6.patch
cargo build --release --locked -p ppvpn-core-lab --bin ne-mem --features ppvpn-core/lab
mkdir -p build/perf
(cd test && CGO_ENABLED=0 go build -trimpath -o ../build/perf/fakenode ./perf/fakenode)
(cd test && CGO_ENABLED=0 go build -trimpath -o ../build/perf/loadgen ./perf/loadgen)
project=$(pwd)
results=$(mktemp -d)
sudo sh -c 'cd "$2"; ulimit -n 16384; exec python3 "$1/tools/perf/ne_mem.py" \
  --ne-mem "$1/target/release/ne-mem" --fakenode "$1/build/perf/fakenode" \
  --loadgen "$1/build/perf/loadgen" --workers 2 --variant ios --no-default --dns-source public \
  --out ios-memory.json' sh "$project" "$results"
```

Run this in an isolated Linux environment with TUN access. The harness temporarily routes TEST-NET-3 to the test tunnel. Keep generated logs and result files outside the repository, as required by `CONTRIBUTING.md`.

## Remaining acceptance

Keep the PR in draft: the public DNS load still needs to pass without lowering its threshold; whole-process RSS exceeds 35 MiB and Apple footprints remain unmeasured; the dependency update needs a Sail release tag. Physical-device validation additionally needs the signed Network Extension entitlement described in #388.
