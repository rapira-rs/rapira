# Large-Payload gRPC Performance Implementation Plan

> **For agentic workers:** Use the `executing-plans` skill to implement this plan task by task. Each task has a separate acceptance check.

**Goal:** Reduce CPU use, payload copies, and allocation costs for large unary gRPC messages.

**Architecture:** Keep Tonic request validation, the existing dispatcher, and one PHP interpreter per worker. Start with response compression controls and the compression backend. Then test local buffer changes against the measured allocation and copy costs.

**Tech Stack:** Rust, Tonic revision `a597b92070dc12d2239923821f86391ad857d910`, Hyper 1.11.1, h2 0.4.19, flate2 1.1.10, miniz_oxide 0.9.1, PHP 8.5.10 NTS, Linux perf.

**Spec:** The request is to profile the gRPC layer on `maindev` with large payloads and produce an improvement plan. The scope, evidence, proposed changes, and acceptance criteria are in this file.

## Global constraints

- NTS only, `build.rs` rejects ZTS. Unix only.
- One interpreter per forked worker. Master is single-threaded, no tokio; workers inherit listener fds.
- MINIT runs once in the master pre-fork so opcache SHM is inherited. Workers exit rather than tear the module down.
- Allocator is mimalloc v3.
- Keep Zend allocation, reference-count operations, and release operations on the PHP thread.
- Keep allocation bailouts inside C isolation functions. Put host logic in Rust where possible.
- Preserve unary cardinality, message limits, metadata, status trailers, cancellation, deadlines, and worker cleanup.
- Put unit tests in the relevant crate. Put PHP integration and e2e tests in `crates/tests`.
- Use flat test tables with an explicit `name` field.

## Measured evidence

### Method

- Host: Fedora 44 KVM guest, 32 virtual CPUs, AMD Ryzen 9 9950X3D CPU model.
- Server: current working-tree snapshot, release optimization, thin LTO, one code-generation unit, debug information, and frame pointers.
- PHP: system `/usr/bin/php-config`, PHP 8.5.10 NTS, `/usr/lib64/libphp-8.5.so`, and `/etc/php.ini`.
- Server CPU affinity: virtual CPUs 0-15. Client CPU affinity: virtual CPUs 16-31.
- Transport: cleartext HTTP/2 over loopback TCP. PHP echoes raw protobuf bytes. The client checks every response byte and the selected response encoding.
- Payloads: 64 KiB, 1 MiB, and 4 MiB data fields, plus 1,900 KiB and 2,100 KiB allocation probes. Protobuf adds four or five bytes. The configured request and response limits are 8 MiB.
- Content: deterministic random bytes and repeated text records. The compression cases cover identity, gzip responses, and gzip in both directions.
- Load: three measurement repetitions per case. Each repetition lasts eight or ten seconds after warmup. The one-worker cases use eight connections and 16 concurrent calls.
- Profiling: `perf record -F 199 -e cycles --call-graph dwarf,16384`, attached to the master and worker threads. Profiles last 12 or 15 seconds.
- Counters: `perf stat` records task-clock, cycles, instructions, context switches, migrations, and page faults. Separate runs measure throughput without perf instrumentation.
- Result: 57 measured runs, 2,659,564 measured calls, and zero call errors. The 14 selected profiles have zero lost samples. Three multi-worker captures were repeated with larger perf buffers after sample loss. All server processes exited successfully, and their error logs were empty.

These are loopback measurements on one VM. The client and server share the memory system. Use the results to rank CPU costs and select experiments. Check deployment throughput on the target network after implementation.

### Throughput without perf instrumentation

These values are medians of three runs. Each case uses one PHP worker.

| Data field | Response encoding | Calls/s | p99 latency |
| --- | --- | ---: | ---: |
| 64 KiB random | Identity | 36,008 | 0.821 ms |
| 1 MiB random | Identity | 3,459 | 8.297 ms |
| 1 MiB random | Gzip | 119 | 338.269 ms |
| 1 MiB repeated text | Gzip | 1,569 | 17.841 ms |
| 4 MiB random | Identity | 667 | 35.229 ms |

### Finding 1: Gzip dominates the compressed cases

For 1 MiB random responses, `miniz_oxide::deflate` functions use **95.14% of sampled cycles**. `compress_inner` alone uses 90.19%. For repeated text, deflate uses 68.93%. With gzip in both directions, repeated-text inflation uses 9.15%, while deflate uses 71.62%.

The request path enables response gzip whenever the client accepts it: `crates/plugins/grpc/src/handler.rs:243-247`. Tonic compresses at level 6 in its response-body polling path. The two `rapira-grpc-io` threads perform that work. See `crates/plugins/grpc/src/lib.rs:73-76` and [Tonic compression](https://github.com/grpc/grpc-rust/blob/a597b92070dc12d2239923821f86391ad857d910/tonic/src/codec/compression.rs#L225-L233).

**Priority:** Give applications an explicit response-compression choice. Then test a faster gzip backend for applications that need compression. A size threshold alone does not distinguish random bytes from compressible text.

### Finding 2: Full-payload copies are a major identity-path cost

`__memmove_avx512_unaligned_erms` uses **28.59% of sampled cycles** for 1 MiB identity messages and 27.22% for 4 MiB. The clean eight-worker profiles show 41.69% and 36.20%, respectively.

The source contains these payload-copy sites:

| Stage | Location | Operation |
| --- | --- | --- |
| Request assembly | Tonic `codec/decode.rs`, `StreamingInner::poll_frame` | Appends HTTP body data into a contiguous buffer |
| PHP request string | `crates/php_sys/rapira_grpc.c:265-270` | `RETURN_STRINGL` allocates and copies the payload |
| PHP response transfer | `crates/php_sys/src/grpc/call.rs:349-355` | `Bytes::copy_from_slice` makes a Rust-owned response |
| Response encoding | `crates/plugins/grpc/src/codec.rs:32-34` | `put_slice` copies the response into Tonic's output buffer |

`RawDecoder::copy_to_bytes` already uses `BytesMut::split_to(...).freeze()` through Tonic's `DecodeBuf`. Preserve this shared-buffer path. The measured copy percentage covers several sites; it is not the saving available from one change.

**Priority:** Test removal of the Rust-to-Tonic response copy. Keep PHP-to-Rust ownership transfer safe. Measure each copy change separately.

### Finding 3: PHP's huge-allocation boundary adds page faults

The counter runs show the following medians:

| Random data field | Page faults/call | Server CPU time/call |
| --- | ---: | ---: |
| 1 MiB | 0.62 | 0.723 ms |
| 1,900 KiB | 2.02 | 1.589 ms |
| 2,100 KiB | 527.97 | 2.035 ms |
| 4 MiB | 1,031.79 | 4.518 ms |

The 1,900-to-2,100 KiB probe increases payload size by 10.5%, but increases CPU time per call by 28.1%. The 4 MiB profile includes `zend_mm_alloc_huge`, `zend_mm_mmap`, `munmap`, and page-fault handling. Page-fault handling has 16.20% inclusive cost in that profile. Inclusive costs overlap and must not be added to the copy costs.

PHP 8.5.10 sets `ZEND_MM_MAX_LARGE_SIZE` to 2 MiB minus 4 KiB. String headers also count toward the allocation size. `zend_string_init` allocates the string and copies its bytes. The source and the size-boundary experiment agree: large `getMessage()` strings cause repeated huge allocations. See [allocation sizes](https://github.com/php/php-src/blob/php-8.5.10/Zend/zend_alloc_sizes.h#L22-L29), [huge allocation and release](https://github.com/php/php-src/blob/php-8.5.10/Zend/zend_alloc.c#L1916-L2002), and [string initialization](https://github.com/php/php-src/blob/php-8.5.10/Zend/zend_string.h#L187-L193).

**Priority:** Test bounded reuse of large request-string storage on the PHP thread. This change needs ownership, memory-limit, and bailout tests.

### Finding 4: HTTP/2 framing and concurrency affect large-message cost

Hyper currently uses its default 16 KiB receive frame size. h2 chains a large DATA payload to its frame header, then flushes that frame through vectored I/O. The 1 MiB identity profile has 36.30% inclusive cost under `do_writev`. See `crates/plugins/grpc/src/serve.rs:46-47` and h2 0.4.19 `src/codec/framed_write.rs:138-169`.

Advertising a **256 KiB receive frame size in the client** improves the instrumented 1 MiB case from 3,310 to 3,851 calls/s: **16.4% higher throughput**. Server CPU time per call falls from 0.723 to 0.643 ms: **11.1% lower CPU cost**. This changes server response framing. The server's receive-frame setting controls the opposite direction.

The eight-worker settings also matter:

| Connections | Concurrent calls | Calls/s | p99 | Server CPU time/call |
| ---: | ---: | ---: | ---: | ---: |
| 64 | 128 | 3,480 | 90.916 ms | 4.340 ms |
| 16 | 16 | 5,157 | 6.158 ms | 1.837 ms |

Both connection count and concurrency change in this comparison. It establishes a better measured operating point for this VM. It does not isolate one setting as the cause. Shared memory pressure and scheduling need further measurement before changing pool defaults.

## Implementation order

### Task 1: Make response gzip an explicit setting

**Files:** `crates/config/src/grpc.rs`, `crates/config/src/lib.rs`, `crates/plugins/grpc/src/lib.rs`, `crates/plugins/grpc/src/handler.rs`, `crates/plugins/grpc/src/tests.rs`, `src/main.rs`, and `crates/plugins/grpc/README.md`.

**Interface:** Add `[grpc.compression.gzip]` with `enabled = true` or `enabled = false`. The default is `false`. Request gzip remains supported. A configured `true` still requires client acceptance. Reject unknown compression settings and invalid value types.

- [x] Add this default-policy test to the existing plugin test module. Run it before changing the handler. Confirm the expected failure for gzip-accepting cases.

```rust
#[tokio::test]
async fn response_compression_requires_server_opt_in() {
    struct Case {
        name: &'static str,
        accept: Option<&'static str>,
    }
    let cases = [
        Case { name: "no_accept_header", accept: None },
        Case { name: "gzip_accepted", accept: Some("gzip") },
        Case { name: "gzip_and_identity_accepted", accept: Some("gzip,identity") },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let message = b"\x0a\x02\0\xff";
        let mut req = request("/example.Echo/Call", frame(message));
        if let Some(accept) = case.accept {
            req.headers_mut().insert("grpc-accept-encoding", accept.parse().unwrap());
        }
        let (headers, body, trailers) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(status(&headers, &trailers).code(), tonic::Code::Ok, "{}", case.name);
        assert!(headers.get("grpc-encoding").is_none(), "{}", case.name);
        assert_eq!(body.as_ref(), frame(message).as_slice(), "{}", case.name);
    }
}
```

- [x] Resolve the nested setting with a disabled default. Pass it through `src/main.rs::grpc_pool`.
- [x] Change the handler's Tonic configuration to the following form.

```rust
let mut grpc = Grpc::new(RawCodec)
    .accept_compressed(CompressionEncoding::Gzip)
    .max_decoding_message_size(self.cfg.max_request_message_size)
    .max_encoding_message_size(self.cfg.max_response_message_size);
if self.cfg.gzip_responses {
    grpc = grpc.send_compressed(CompressionEncoding::Gzip);
}
```

- [x] Extend the flat configuration table with explicit values, invalid types, omitted tables, and unknown fields. Check resolved values and validation failures.
- [x] Set `gzip_responses: true` in the existing compression-limit test. Add cases for an accepted gzip request with an identity response and an opted-in server with an identity-only client.
- [x] Run `cargo test -p rapira_config` and `cargo test -p rapira_grpc`. Run the gRPC e2e suite with system PHP.
- [x] Make a candidate copy of the profiling harness. Set `grpc.compression.gzip.enabled = true` in its gzip-case configurations. Set it to `false` for the policy probe. Keep the original harness with the baseline artifacts.
- [x] Separate accepted and expected compression in the candidate client. Add `PROFILE_EXPECT_ENCODING`; use it only for response validation. The client continues to advertise gzip during the identity-response policy probe.

```rust
let expected_encoding = std::env::var("PROFILE_EXPECT_ENCODING")
    .unwrap_or_else(|_| if receive_gzip { "gzip" } else { "identity" }.to_owned());
```

Clone `expected_encoding` into each client task. Replace its `(actual_encoding == "gzip") != receive_gzip` check with `actual_encoding != expected_encoding`. Keep the full payload comparison. Rebuild the client once and use that binary for the remaining comparisons.

- [x] Repeat the random-payload workload with a gzip-capable client and `grpc.compression.gzip.enabled = false`. Verify identity encoding, exact payload equality, and performance within 5% of the identity-client baseline.
- [x] Document the setting and the CPU-versus-bandwidth tradeoff. Leave changes uncommitted, as requested.

### Task 2: Test a faster gzip backend

**Files:** `crates/plugins/grpc/Cargo.toml` and `Cargo.lock`. Extend `crates/plugins/grpc/src/tests.rs` only for missing compression edge cases.

**Interface:** Preserve Tonic's gzip behavior and level 6. Select flate2's `zlib-rs` backend through Cargo features. This is a measured candidate; acceptance depends on the results below.

- [x] Record the current explicit-gzip baseline after Task 1. Use random and repeated-text 1 MiB messages in both compression directions.
- [x] Test this flate2 backend selection. Restore the original dependency after the compressed-size gate fails.

```toml
flate2 = { version = "1", default-features = false, features = ["zlib-rs"] }
```

- [x] Run `cargo tree -p rapira_grpc -e features -i flate2`. Confirm candidate backend selection against flate2 source.
- [x] Run the candidate's compression, size-limit, and malformed-input plugin tests. Verify wire compatibility with the unchanged profiling client. Run final gRPC e2e tests after restoring miniz.
- [x] Repeat three counter runs, three uninstrumented runs, and one profile for each gzip case. Compare CPU time per call, p99, output size, and exact decoded bytes.
- [x] Apply the acceptance gates: at least 20% lower random-response CPU cost, no text CPU regression, and at most 5% compressed-size growth. Reject both tested backends because text output grows by 97.2%.
- [x] Retain the original dependency selection. No new compression dependency remains.

### Task 3: Reuse large PHP request-string storage

**Files:** `crates/php_sys/src/grpc/call.rs`, a new `crates/php_sys/src/grpc/message.rs`, `crates/php_sys/src/grpc/mod.rs`, `crates/php_sys/rapira_grpc.c`, `crates/php_sys/module.c`, and the relevant FFI declarations. Tests belong in `crates/tests/tests/grpc_dispatcher.rs` and a new `crates/tests/fixtures/grpc/message_buffer.php`.

**Interface:** Keep PHP `getMessage(): string`. Add a PHP-thread-local `MessageBuffer` with a Zend string pointer and an allocation capacity. Keep at most one cached allocation per worker. Use the current copy path for allocations below Zend's huge-allocation boundary.

- [x] Add flat ownership cases named `binary_bytes`, `retained_across_receive`, `caller_mutation`, `repeated_get`, `large_then_small`, and `growing_message`. Use literal expected strings or digests derived from the input fixture. Run these semantic tests against the baseline.
- [x] Record the allocation-performance failure: the baseline 4 MiB case exceeds 100 page faults per call after warmup. Use the profiling harness outside timing-sensitive CI tests.
- [x] Implement allocation reuse only when the cache owns the string's only reference and its capacity is sufficient. Otherwise, release the cache's reference and allocate new storage. Return a separate Zend reference to the caller.
- [x] Copy the request bytes into the selected storage. Set the string length and trailing NUL. Clear the cached hash and content-derived string flags before reuse. Follow `zend_string_separate` and `zend_string_forget_hash_val` in php-src.
- [x] Keep allocation and bailout isolation in the C shim. Hold Rust borrows only across calls that cannot bypass Rust unwinding. Keep the cache in Rust on the PHP thread.
- [x] Release the cache in RSHUTDOWN while the Zend heap is active. In `grpc::reclaim`, clear any remaining raw cache pointer without a Zend destructor. Verify normal shutdown and shutdown bailouts against tests and php-src.
- [x] Run the ownership cases and existing `allocation_bailout_releases_native_snapshots_and_open_reply`, `exceptions_bailouts_and_exit_release_calls_then_recycle`, and cancellation tests. Confirm PHP memory-limit enforcement and a correct next call after each failure.
- [x] Check direct argument use and a retained PHP `$payload` variable. Report the performance of both use patterns.
- [x] Repeat the 1,900 KiB, 2,100 KiB, and 4 MiB profiles. Pass the page-fault, 4 MiB CPU, and 64 KiB regression gates. Account for retained buffer memory.
- [x] Retain the accepted implementation. Leave changes uncommitted, as requested.

### Task 4: Remove the identity response's extra Rust buffer copy

**Files:** `crates/plugins/grpc/src/codec.rs`, `crates/plugins/grpc/src/handler.rs`, `crates/plugins/grpc/src/tests.rs`, and `crates/tests/tests/e2e/grpc.rs`.

**Interface:** Add `IdentityBody::new(message: Bytes) -> Result<IdentityBody, Status>`. Its body emits a five-byte gRPC prefix, the owned `Bytes`, and an OK status trailer. Tonic continues to validate requests and produce compressed responses.

- [x] Add flat body cases for empty bytes, binary bytes, a payload at the response limit, initial metadata, repeated trailers, and client cancellation before the body is consumed. Retain the existing over-limit and rich-error tests.
- [x] In `PhpUnary`, retain a cheap `Bytes` clone in an additional `OnceLock<Bytes>` after a successful result passes its size check. Continue returning the normal Tonic response.
- [x] Name the additional payload slot `reply_message`. Apply this replacement before metadata attachment.

```rust
if !response.headers().contains_key("grpc-encoding") {
    if let Some(message) = reply_message.into_inner() {
        response = match IdentityBody::new(message) {
            Ok(body) => response.map(|_| tonic::body::Body::new(body)),
            Err(status) => status.into_http(),
        };
    }
}
```
- [x] Implement the body as the following state sequence. Validate that the length fits the gRPC 32-bit envelope before making the prefix.

```text
prefix:   0x00 || message_length.to_be_bytes()
message:  the existing owned Bytes allocation
trailers: grpc-status: 0
end
```

- [x] Keep metadata attachment, deadline handling, and the in-flight response guard around the replacement body. Verify trailer merging and deadlines under HTTP/2 flow control.
- [x] Run all plugin and gRPC e2e tests. Uninstrumented paired controls pass the 5% identity CPU reduction gate at 1 MiB and 4 MiB. Gzip counter results stay within 3%. Record the smaller instrumented 1 MiB gain separately.
- [x] Retain the accepted implementation. Leave changes uncommitted, as requested.

### Task 5: Record and validate transport settings

**Files:** `crates/plugins/grpc/README.md` and the profiling client. Change `crates/plugins/grpc/src/serve.rs` only if the separate inbound-frame experiment below passes.

**Interfaces:** Use the client's existing `Endpoint::max_frame_size` API. Use the server's existing Hyper builder API for the inbound experiment. Keep connection count and concurrent-call count as separate workload inputs.

- [x] Document the measured client receive-frame setting with this code.

```rust
let endpoint = Endpoint::from_shared(uri)?
    .max_frame_size(256 * 1024u32);
```

- [x] Run a flat matrix that changes connection count and concurrency separately: `(16, 16)`, `(16, 64)`, `(64, 64)`, and `(64, 128)`. Use eight workers and both 1 MiB and 4 MiB payloads.
- [x] Record private memory, CPU time per call, throughput, and p99.
- [x] In a separate candidate build, set `builder.max_frame_size(256 * 1024)` in `serve.rs`. Keep the client receive setting fixed.
- [x] Reject the server default change after the 4 MiB CPU gate fails. Stop this candidate before the slow-sender and mixed-load acceptance checks.
- [x] Document the measured client setting and workload. Keep the server default. Leave changes uncommitted, as requested.

## Final checks for accepted changes

- [x] Run `make test PHP_CONFIG=/usr/bin/php-config` on `maindev`: 634 tests passed, including 85 e2e tests and all 11 gRPC e2e tests.
- [x] Run workspace Clippy with warnings denied, including the e2e feature for the test crate: both commands passed.
- [x] Run `cargo fmt --all --check` and `git diff --check`: passed after an import-only formatting correction.
- [x] Repeat the profile matrix with the same system PHP, CPU sets, client binary, deterministic payloads, and logging level.
- [x] Compare uninstrumented throughput with uninstrumented throughput. Compare counter measurements with counter measurements.
- [x] Verify zero response mismatches, zero gRPC errors, zero lost samples in all 17 candidate/control profiles, and 229 successful server shutdowns.
- [x] Record measured gains for each accepted change separately. Keep work on the user-selected `feature/grpc` branch and leave it uncommitted.

## Artifacts and reproduction

The complete run is on `maindev`:

```text
/home/valery/projects/opensource/github/rapira/benchmarks/results/grpc-perf-20260918T104746Z/
```

- `source-manifest.json` and `sources.tar.gz`: exact profiled working-tree snapshot.
- `build-metadata.json` and `machine.json`: release flags, PHP selection, compiler, and host details.
- `profile.py`, `client/`, `proto/`, and `dispatcher.php`: workload and profiling tools.
- `runs/baseline/` and `runs/baseline-rest/`: initial counter runs and profiles.
- `runs/unprofiled-control/`: throughput runs without perf instrumentation.
- `runs/allocation-and-frames/`: allocation-boundary, frame-size, and lower-concurrency experiments.
- `runs/reprofile-w8/`: clean replacement multi-worker profiles with 4 MiB perf ring buffers.
- `analysis.json` and `verification.json`: aggregated measurements and completion checks.
- `profile-artifacts.tar.gz`: source snapshot, scripts, metadata, and text reports. Raw `perf.data` files remain in their run directories.

Run these commands from the artifact directory. Use a new label for each run.

```bash
python3 profile.py --label candidate-counters \
  --only 1m_random_identity_w1 4m_random_identity_w1 \
         1m_random_response_gzip_w1 1m_text_response_gzip_w1 \
  --duration 10 --repetitions 3 --warmup 3 --profile 15

python3 profile.py --label candidate-throughput \
  --only 1m_random_identity_w1 4m_random_identity_w1 \
         1m_random_response_gzip_w1 1m_text_response_gzip_w1 \
  --no-stat --duration 10 --repetitions 3 --warmup 3 --profile 0

perf report --stdio --no-children --no-inline -g none \
  -i runs/baseline/1m_random_response_gzip_w1/perf.data
```

For candidate comparisons, make `profile.py::BINARY` point to the candidate release binary and record its hash. Keep the original binary and its measurements. The `gzip_bytes` value in `case.json` is a Python reference compression result used to check input compressibility; it is not a measurement of Tonic's response size.

## Execution results

Candidate artifacts are on `maindev` in `/home/valery/projects/opensource/github/rapira/benchmarks/results/grpc-improvements-20260918/`. Each accepted change has a source manifest and a separate release binary. Paired comparisons alternate the two binaries for three runs per case. Changes remain uncommitted on the user-selected branch.

### Response policy

The nested configuration passes local config, plugin, and gRPC e2e tests. With response gzip disabled, a gzip-capable client received exact identity responses at 3,002 calls/s. An identity-only client reached 3,045 calls/s. The 1.43% difference passes the 5% gate. Explicit response gzip remains available.

### Compression backend decision

Keep the current miniz backend. `zlib-rs` reduced random-response CPU time per call by 21.6% and text-response CPU time by 55.2%. However, the compressed text response grew from 3,670 to 7,238 bytes (+97.2%). An isolated `zlib-ng` build produced the same 7,238-byte result. Both fail the 5% compressed-size gate. The separate client decoded every response correctly. See `backend-acceptance.json` and `runs/backend-counters/` in the candidate artifact directory.

### Connection and concurrency controls

The baseline control matrix uses eight workers and three counter runs per case. Private memory is the total for the master and workers. Both payload sizes favor 16 connections and 16 concurrent calls in this VM. Raising concurrency from 16 to 64 at a fixed 16 connections increases CPU cost, memory use, and p99 latency. Increasing connections from 16 to 64 at fixed concurrency has different throughput effects for the two payload sizes.

| Payload | Connections | Concurrent calls | Calls/s | CPU ms/call | p99 ms | Private MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 MiB | 16 | 16 | 4,258 | 2.309 | 6.82 | 161.23 |
| 1 MiB | 16 | 64 | 3,294 | 3.527 | 44.14 | 223.12 |
| 1 MiB | 64 | 64 | 3,622 | 3.805 | 42.91 | 231.34 |
| 1 MiB | 64 | 128 | 3,279 | 4.418 | 97.90 | 304.84 |
| 4 MiB | 16 | 16 | 695 | 14.474 | 40.68 | 293.00 |
| 4 MiB | 16 | 64 | 605 | 22.160 | 189.85 | 525.20 |
| 4 MiB | 64 | 64 | 595 | 24.604 | 216.78 | 524.48 |
| 4 MiB | 64 | 128 | 580 | 26.016 | 446.44 | 830.34 |

### PHP request-buffer reuse

Accepted after the ownership review, all nine dispatcher tests, and the paired counter measurements. The three allocation-boundary profiles have zero lost samples.

| Payload | CPU reduction | Page faults/call before | Page faults/call after |
| --- | ---: | ---: | ---: |
| 64 KiB | -2.98% | 0.011 | 0.011 |
| 1,900 KiB | 0.77% | 2.19 | 2.20 |
| 2,100 KiB | 16.76% | 528.32 | 2.30 |
| 4 MiB | 14.95% | 1,033.85 | 9.51 |

The direct-call pattern passes the planned CPU and page-fault gates. In the separate retained-variable case, reassignment keeps the previous PHP value live while `getMessage()` runs. That case retains approximately 1,033 faults/call and had 3.78% higher CPU cost. Direct argument use or an explicit `unset($payload)` before the next read permits reuse. The one-buffer cache can retain the largest accessed huge message until the worker cycle ends. Budget approximately 4 MiB plus one page for a cached 4 MiB message on this host. The measured total private-memory increase was 4.10 MiB in the direct-use 4 MiB case. See `cache-acceptance.json`.

### Server receive-frame experiment

Keep the current server receive-frame default. A separate 256 KiB candidate reduced request-heavy CPU cost by 14.07% at 1 MiB but only 6.55% at 4 MiB. The second result misses the 10% gate. Small-call p99 improved from 570 to 552 microseconds. Private memory increased by 33.66 MiB at 1 MiB and 14.69 MiB at 4 MiB. The experiment stopped at the CPU gate, so it does not claim slow-sender or mixed-load acceptance. The client receive-frame guidance remains a separate measured setting. See `runs/inbound-frame-counters/summary.json`.

### Identity response-copy removal

Accepted after all 39 plugin tests, all 11 gRPC e2e tests, ownership review, and paired measurements. The implementation emits the gRPC prefix, the existing Rust-owned payload, and status trailers. It uses an unspecified HTTP body length so a response deadline can stop DATA and send a deadline status under HTTP/2 flow control.

The acceptance comparison uses three alternating uninstrumented runs per binary. CPU time comes from `/proc` process counters. Both primary payload sizes pass the 5% CPU reduction target. Each comparison uses the preceding cache-only binary as its baseline.

| Payload | Calls/s before | Calls/s after | Throughput gain | CPU reduction | p99 before | p99 after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 32 bytes | 74,097 | 73,783 | -0.42% | -0.48% | 0.386 ms | 0.389 ms |
| 64 KiB | 33,320 | 34,824 | 4.51% | 5.01% | 0.915 ms | 0.886 ms |
| 1 MiB | 3,225 | 3,411 | 5.79% | 6.60% | 8.873 ms | 8.439 ms |
| 4 MiB | 592 | 668 | 12.91% | 9.76% | 39.860 ms | 37.770 ms |

The separate perf-counter runs measured CPU reductions of 1.82% at 1 MiB and 11.79% at 4 MiB. The instrumented 1 MiB result misses the 5% target; preserve it with the uninstrumented result. The boundary cases saved 7.03% at 1,900 KiB and 6.89% at 2,100 KiB. A 256 KiB client receive-frame limit still gave only 1.90% incremental CPU saving in the instrumented 1 MiB comparison. This does not establish frame size as the cause of the measurement difference.

The selected profiles show memory-copy self cost falling from 27.65% to 23.06% at 1 MiB and from 37.61% to 30.24% at 4 MiB. These are fractions of sampled cycles, not percentages of total CPU saved. All four before/after profiles have zero lost samples. Gzip CPU cost changed by -1.70% for random responses and -0.08% for repeated text, within the 3% gate. See `identity-acceptance.json`, `copy-profile-analysis.json`, and `runs/identity-throughput/`.

### Final combined checks

The final matrix covers one and eight workers, both compression directions, allocation-boundary sizes, a gzip-capable client with identity responses, and retained PHP variables. The candidate/control artifact set contains 293 measured runs and 15,805,183 successful calls. There were zero payload or encoding errors, 229 clean server shutdowns, 210 exact wire probes, and 17 profiles with zero lost samples. The original profiling run remains in its separate artifact directory.

The first unpaired eight-worker result had a higher 4 MiB p99 at 128 concurrent calls. A final alternating paired control did not reproduce that regression. The table compares the configured baseline with both accepted buffer changes. These are counter-run medians, separate from the uninstrumented table above.

| Connections | Concurrent calls | Calls/s before | Calls/s after | CPU reduction | p99 before | p99 after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | 16 | 720 | 819 | 18.23% | 39.316 ms | 33.747 ms |
| 64 | 128 | 563 | 637 | 12.65% | 578.170 ms | 566.098 ms |

`make test PHP_CONFIG=/usr/bin/php-config` passed 634 tests, including 85 e2e tests and all 11 gRPC e2e tests. Workspace Clippy and e2e Clippy passed with warnings denied. Formatting and whitespace checks passed. CodeRabbit and the final source review each returned zero findings. The final source uses miniz and the original server receive-frame default. The user-selected `feature/grpc` branch and its index are preserved; all new work remains uncommitted.

The candidate artifact directory contains `verification.json`, `final-checks.json`, all source manifests, immutable candidate binaries, and the raw profiles. `improvement-artifacts.tar.gz` contains source snapshots, benchmark tools, measurements, and text profiles. `artifact-checksum.json` records its SHA-256 checksum. Raw `perf.data` files stay on `maindev`.

### Empty and one-byte message follow-up

The ext-php-rs source comparison identified Zend's shared empty and one-byte strings. `getMessage()` now uses `RETURN_STRINGL_FAST` below the large-buffer threshold. A probe retained 256 reads in a preallocated PHP array. Additional Zend string storage fell from 8,192 bytes to zero for empty, ASCII, NUL, and `0xff` messages. The two-byte control still used 8,192 bytes. Mutation of one returned value preserved the other values and subsequent reads.

The empty-message load probe also found an extra zero-length DATA frame in identity responses. H2 0.4.19 closes a connection after 100 non-final empty DATA frames. A new regression test reproduced the failure on the 101st empty reply. The body now sends the five-byte gRPC prefix and status trailers for empty messages. The test passes 128 empty replies and 128 binary replies on reused connections.

The fast-string comparison uses the frame-corrected binary as its baseline. Three alternating uninstrumented runs per binary used one worker, eight connections, and 16 concurrent calls. The measured throughput changes were -0.71% for empty messages, +0.39% for one-byte messages, and +0.57% for the 32-byte control. CPU cost changed by +0.56%, -0.34%, and -0.26%. These results confirm the allocation saving but do not establish a throughput gain.

Follow-up verification passed 40 plugin tests, nine PHP dispatcher tests, all 11 gRPC e2e tests, targeted Clippy with warnings denied, formatting, and whitespace checks. CodeRabbit returned zero findings across the three changed source files. The 18 measured runs completed 13,844,471 calls with zero errors. All 28 probe and measurement servers exited cleanly, and all 28 wire probes matched the payload. Artifacts are on `maindev` in `/home/valery/projects/opensource/github/rapira/benchmarks/results/grpc-fast-string-20260919/`. Source snapshots and the `emptyframes` and `faststrings` binaries remain in the main candidate artifact directory.
