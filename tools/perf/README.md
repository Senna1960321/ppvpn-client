# Performance checks, tiers A and B

The tier A and B comparisons run by hand on the test host (see `docs/testing.md`). The separate Network Extension memory check also runs in CI, as described below. These scripts measure:

- release binary sizes;
- the engine process's RSS, both idle and under a fixed load;
- allocations;
- apply and kernel-switch time.

`report.py compare` compares them with an earlier run and with the Go 0.5.21 baseline in `testdata/perf/`; the Go engine to measure against is the v0.5.21 release file.

| File | Role |
| --- | --- |
| `measure.py` | Drives an engine that serves Core API v1 on a Unix socket with the `ppvpn-core serve` command line and log format: `ppvpn-core` today, the Rust engine's lab binary later. It starts the engine, drives it over the API, and reads `/proc` and the engine's log. It runs on Linux only. The TUN instance needs root and runs in its own network namespace. |
| `ne_mem.py` | Peak memory in the iOS Network Extension's shape (#388): drives `ne-mem` (`crates/core-lab/src/bin/ne-mem.rs`), which holds the Engine in-process as the extension will (TUN only, mobile platform, no local proxy) and reports `phys_footprint` on macOS and anonymous RSS on Linux per scenario, for core's own sail options and for variants of sail's tuning (`VARIANTS`; ne-mem built with ppvpn-core's `lab` feature). A scenario whose load failed is marked ✗. As root, on macOS and Linux; ci.yml's macos and linux jobs run it when the harness or `Cargo.lock` changes. |
| `dhat_top.py` | The heap at its peak by allocation site, from a dhat heap profile (`ne_mem.py --dhat`, `ne-mem` built with its `dhat` feature): bytes live at t-gmax by crate and by the first frame past the allocator, and the largest sites' stacks, as Markdown. ci.yml's linux job runs it on the downloads and on 500 connections. |
| `report.py` | `collect` (`PERF` lines and sizes into JSON), `merge`, `check` (numbers only, since the file is public) and `compare` (thresholds). Adapted from Sail's. |
| `../../test/perf/fakenode` | The stand-in node, on loopback: Shadowsocks 2022, AnyTLS (with a certificate generated at start, trusted through `SSL_CERT_FILE`) and an echo sink. |
| `../../test/perf/loadgen` | The fixed loads through the local proxy. |
| `internal/runtime/perf_alloc_test.go` at tag v0.5.21 | The Go engine's allocation counts (the Go core is no longer on main). They compare only with the same engine. |

## Network Extension memory (#388)

`ne_mem.py --variant ios` combines Sail's mobile profile with `inbound.max_connections=128`, matching the iOS host. The Linux and macOS CI jobs use two workers and compare `default`, `cap`, `ios` and `budget`; `default` is the mobile profile without the host's cap. Both jobs build `ne-mem` with `--profile mobile --features ppvpn-core/lab`, matching the shipped mobile build profile. The Python acceptance tests and Go load-generator/DoT fixture tests run before the measurements.

A failed load in `default` exits nonzero; with `--no-default`, the first selected variant takes its place. The `ios` variant is always checked too. Its connection scenarios require complete echoes from every requested stream up to 128. Above the cap, exactly 128 streams must complete without write errors or stalls; refused excess streams remain marked ✗ in the report. TCP handshakes alone do not count as completed loads. The existing `--dhat` exception only permits a failed DNS load, which remains visible.

The default DNS load sends 2,000 distinct queries at 400 queries/s through the real TUN, Sail DNS client, selected proxy node and verified TLS. The fake node redirects only TCP port 853 at `1.1.1.1`, `8.8.8.8` and `9.9.9.9` to a local pipelined DoT responder. Each response is NXDOMAIN after 100 ms, creating repeatable concurrent DNS work without public resolver rate limits. Other destinations and rule-set HTTPS downloads keep their normal path. The response validator still checks unique IDs, matching questions, opcode, truncation and successful resolution status; it waits up to 11 seconds after the last query and fails below 95%. Per-run fixture query/reply counters must also cover the validated replies, so bypassing the fixture cannot pass.

Before measurement, a separate `ne-mem --export-trust-bundle` process loads roots using Sail's native certificate loader, adds the generated fixture CA and writes a temporary bundle. This preserves macOS/Linux native trust and inherited `SSL_CERT_FILE`/`SSL_CERT_DIR` configuration. Only the measured child receives the bundle; no system trust settings change, and export allocations are outside that process. Empty or invalid native/fixture bundles fail clearly. TLS certificate verification remains enabled.

Use `--dns-source public` for a separate public-path diagnostic. It omits the redirect and fixture CA, labels the output as public, and keeps the same response acceptance threshold. Its resolver latency and availability are uncontrolled, so its memory numbers are not interchangeable with the fixed-delay load. A passing local load does not establish that the public path is fixed. The default local load models a 100 ms resolver, not every possible upstream delay or timeout.

The 35 MiB core budget is reported with ⚠ when exceeded, not enforced by the load-success check. A passing load check is not proof that the memory budget is met. This harness excludes the Swift host; iPhone Network Extension acceptance still needs the real extension and its total process footprint.

## Metrics

All metrics use the same profile: one Shadowsocks 2022 node and one AnyTLS node, both on loopback.

- **standard instance** (local proxy, no TUN):
  - `idle_rss_kb`;
  - `<ss|anytls>.<stream|churn>_peak_rss_kb`. *stream* is 8 connections, 50 Mbit/s in total, 30 s, echoed back. *churn* is 200 new connections a second, 4 KiB each way, 30 s.
  - `*_after_rss_kb`, read 30 s after each load. Recorded only.
  - `apply_total_ms` and `kernel_switch_ms`, from an apply that switches kernels.
- **tun instance**: `idle_rss_kb`.
- **alloc** (Go engine only):
  - `<proto>.allocations_per_mib` and `allocated_bytes_per_mib` over a 64 MiB stream;
  - `allocations_per_connection` over 500 connections.
- **size**: the six desktop release binaries of v0.5.21 (built by that tag's `release.yml`).

## Caveats

- **RSS and size are the engine's alone.** The Rust engine runs inside the hosts' processes, so the hosts' total RSS and installer size are Desktop's measurements (#214). Against Go, sizes are a trend only.
- **The fake node's certificate is trusted only on Linux.** AnyTLS trusts it through `SSL_CERT_FILE`, which Go honours on Linux but not on macOS, so the measurement runs on Linux. The Rust engine's lab binary must accept a CA file the same way, for example a flag or `SSL_CERT_FILE` with rustls-native-certs, for this script to measure it.
- **Allocations do not compare across engines.** Go's runtime and a Rust allocator count differently.
- **Out of scope for tier A:** CPU, throughput and latency depend on the runner. Tier B measures them, below.

Run it locally on a Linux host, as root for the TUN instance:

```sh
sudo -E tools/perf/measure.py --engine-bin build/ppvpn-core --fakenode "$FAKENODE" --loadgen "$LOADGEN" --rounds 3
```

## Tier B

CPU, throughput and latency (#214's performance thresholds) depend on the machine, so they are not measured in CI. They are measured by hand on a dedicated Linux host, with the same scripts: `measure.py --tier b`. The numbers go into an issue, not into the repository.

- **Pairs.** Each round measures every engine given with `--engine`, in turn (Go, Rust, Go, Rust, ...), in one run. Go 0.5.21 and the Rust engine are therefore always compared on the same machine on the same day. Hosts are rebuilt, so numbers from different runs or days are not compared.
- **Cores.** `--engine-cpus`, `--load-cpus` and `--node-cpus` pin the engine, loadgen and fakenode to their own cores (`taskset -c`), so that the load and the fake node do not take the engine's CPU. Give them disjoint sets within the cores the job was granted.
- **Parts.** Ask for the cores a part uses, not more. `--part light` (idle CPU, latency, connections, CPU at 100 Mbit/s) needs two physical cores: one for the engine, one shared by loadgen and fakenode. `--part throughput` (unpaced streams) keeps all three busy, and the fake node (sing-box serving the proxy protocol) needs the most: ask for 8 CPUs, give the engine one physical core, loadgen one, and fakenode two. With fakenode on one core it capped AnyTLS with 8 connections for both engines (#193), and the comparison said nothing. `tput<n>_load_limited` is 1 when the engine had CPU to spare (under 80% of the CPUs it was given) while loadgen or fakenode was near its limit (85% of theirs). Such a number is the load side's; report.py does not judge it, except that a capped new number that is still not lower than the baseline passes (it is a lower bound). Run both parts on the same day, and collect their logs together.
- **Where.** The first output line, `ENV {...}`, records the CPU model, cores, memory, kernel, date and the pinning, plus `--label` values: give the job's id, for example `--label hostq_job=<id>`. The report quotes it.
- **What.** The standard instance only (local proxy), against the fake node on loopback. Nothing leaves the host.
  - idle CPU and context switches over 60 s;
  - throughput with 1 and 8 connections, unpaced, 20 s each, per protocol (`tput1_mbit`, `tput8_mbit`);
  - CPU at a paced 100 Mbit/s (`cpu100_pct`, 100 = one core);
  - 64-byte round trips through the proxy against direct ones to the sink (`extra_p50_us`, `extra_p99_us`);
  - new connections until their first byte comes back (`connect_p50_us`).
- **Thresholds** (`report.py`, against the Go engine of the same run): throughput at least 95%, extra latency p50 at most Go + 2 ms and p99 at most 110%, connection p50 at most 110%, CPU at 100 Mbit/s at most 110%, idle CPU not above Go's.
- **Not here:**
  - the TUN path's latency, until the Rust engine opens its TUN;
  - a weak network (G4): tier C, below;
  - real nodes and the 24 h soak (G6);
  - macOS wakeups (powermetrics) and the hosts' total RSS (Desktop, G5).
- **AnyTLS** trusts the fake node's certificate through `SSL_CERT_FILE`. The Go engine honours it on Linux. The Rust engine goes through sail's system store (`rustls-native-certs`), which honours it too, because the translation does not choose another store.

```sh
B="--tier b --rounds 3 --engine go=build/ppvpn-core --engine rust=target/release/ppvpn-core-lab --fakenode $FAKENODE --loadgen $LOADGEN"
tools/perf/measure.py $B --part light --engine-cpus 2,3 --load-cpus 4,5 --node-cpus 4,5 --label job=<id> | tee light.log
tools/perf/measure.py $B --part throughput --engine-cpus 2,3 --load-cpus 4,5 --node-cpus 6,7,8,9 --label job=<id> | tee tput.log
for e in go rust; do tools/perf/report.py collect --sha "$(git rev-parse HEAD)" --engine $e light.log tput.log > $e.json; done
tools/perf/report.py compare rust.json --baseline go.json
```

## Tier C: a weak network (G4)

`measure.py --tier c` runs the engines as tier B does, Go and Rust in turn, under one level each of loss, delay and jitter (#214's G4). It needs root: it runs itself again inside a network namespace of its own, and the host's networking is not touched. It is a one-off acceptance check before the switch, run by hand on a Linux host, never in CI; the results go into #214.

- **The impaired link.** The namespace's loopback has a 1500-byte MTU, segmentation offload off (when `ethtool` can turn it off: `offload_off` in the `ENV` line), and a netem qdisc for the packets to and from the fake node's proxy ports only, both ways. The engine-to-node link is impaired. Loadgen's link to the local proxy and the node's to the sink are not.
- **Conditions** (`--netem`, default all): `none`; `loss`, 1% each way; `delay`, 50 ms each way (100 ms more per round trip); `jitter`, 50 ms ± 20 ms each way, normally distributed, which reorders packets.
- **What**, per condition and node, each load `--c-load-seconds` (20 s): round trips (`rtt_p50_us`, `rtt_p99_us`), throughput with 8 unpaced connections (`tput8_mbit`), new connections until their first byte comes back (`connect_p50_us`, `connect_p99_us`), the connections that failed (`connect_failed`, `stream_failed`), and the throughput connections where one 16 KiB write made no progress for 10 s, as the engine took none of it (`stream_stalled`). Slow is not stalled: under jitter, each hop can hold megabytes that take longer than 10 s to drain after the load, while every write still moves. Failures and stalls are counted rather than stopping the run.
- **Thresholds** (`report.py`, against Go in the same run): the tier B ones that apply (throughput at least 95%, connection p50 at most 110%), and no more failed or stalled connections than Go. Round trips are recorded only: under netem they are mostly the link's.

```sh
# Go 0.5.21 as released, checked against the release's SHA256SUMS.
curl -fsSL -o ppvpn-core-0.5.21 https://github.com/peakpassvpn/ppvpn-client/releases/download/v0.5.21/ppvpn-core-linux-amd64
echo "04e00bbb526d85350814413432acf2bc6bd5efe2abc00de56cfa2da05da7896a  ppvpn-core-0.5.21" | sha256sum -c - && chmod +x ppvpn-core-0.5.21
C="--tier c --rounds 3 --engine go=./ppvpn-core-0.5.21 --engine rust=target/release/ppvpn-core-lab --fakenode $FAKENODE --loadgen $LOADGEN"
sudo -E tools/perf/measure.py $C --engine-cpus 2,3 --load-cpus 4,5 --node-cpus 6,7 --label job=<id> | tee netem.log
for e in go rust; do tools/perf/report.py collect --sha "$(git rev-parse HEAD)" --engine $e netem.log > $e.json; done
tools/perf/report.py compare rust.json --baseline go.json
```

## The long run (G6)

`soak.py` runs one engine for a day under a repeating load and samples it from outside (#214's G6). It is a one-off acceptance check before the switch, run by hand as root on a Linux host and never in CI. The results go into #214.

- **Side by side.** Go and Rust run at the same time as two jobs, each in a network namespace of its own so that their ports do not clash. Each job gets cores of its own: the engine on one physical core, the fake node and loadgen on another.
- **The load.** A 10-minute cycle, on both nodes at once:
  - streams: 8 connections per node, 20 Mbit/s in total, echoed, 180 s;
  - new connections: 50 a second per node, 60 s;
  - round trips: 30 s;
  - idle for the rest of the cycle.

  Every hour an apply-profile switches kernels.
- **Samples.** Every 30 s: RSS, HWM, open descriptors, threads, the engine's TCP sockets, and CPU (recorded only).
- **The verdict** (`soak.py summarize`): no leak means the idle samples do not climb. It compares the hourly medians of an early window of hours with a late one, and it checks that descriptors, threads and sockets return to the same values when idle. Failed and stalled connections are counted too.

```sh
sudo -E tools/perf/soak.py run --engine go=./ppvpn-core-0.5.21 --fakenode "$FAKENODE" --loadgen "$LOADGEN" --out soak/go \
  --engine-cpus 2,3 --load-cpus 4 --node-cpus 5 &
sudo -E tools/perf/soak.py run --engine rust=target/release/ppvpn-core-lab --fakenode "$FAKENODE" --loadgen "$LOADGEN" --out soak/rust \
  --engine-cpus 6,7 --load-cpus 8 --node-cpus 9 &
wait
tools/perf/soak.py summarize soak/go soak/rust
```
