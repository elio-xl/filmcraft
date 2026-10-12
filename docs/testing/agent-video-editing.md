# Agent video editing qualification

P0 acceptance implements the [requirements](../requirements/agent-video-editing.md) using actual
stdio MCP requests to the CLI. It does not require a model to run in CI. Visual model compatibility
is recorded separately and cannot be inferred from a passing transport test.

## Reproduce the automated scenario

```sh
cargo test --release -p filmcraft-cli --test agent_editing -- --nocapture
```

`agent_editing_round_trip` generates original procedural fixtures under
`target/fixtures/agent-editing/roundtrip-<pid>/`. No external media or FFmpeg is required and no binary
fixtures are committed. Each generated asset has an attribution sidecar; `manifest.json` documents
the parameters and licence.

| Fixture | Parameters |
|---|---|
| Three source MOVs | 1280×720, 25 fps, 250 frames each, FilmCraft ProRes Proxy encoder; video only |
| Visual content | Red background / white circle, green background / white triangle, blue background / white square; scene changes at source frames 100 and 200, order rotated by clip id |
| Visible numbers | Original seven-segment digits: clip id 1–3 followed by exact zero-based frame number 000–249 |
| Resolution marker | Two-pixel checker at x=1040..1199, y=520..599; finer than a 512-pixel thumbnail can retain |
| Closing image | 1280×720 opaque magenta PNG |
| Music | 30 seconds, mono PCM16 WAV, 48 kHz, generated sine at 440 + 20×floor(seconds) Hz; each second is identifiable |

The client imports all five files, inspects capabilities and versions, requests source thumbnails,
checks cache reuse and constructs a 1280×720/25 fps sequence. It takes source seconds 1–10 from each
video (nine seconds each), places the closing PNG at seconds 27–30, adds a three-second `Agent cut`
title and the thirty-second music track. It previews, saves `edit.fcproj`, reopens it through
`file.open`, exports `edit.mov` and independently opens that file through the codec interface.

Assertions cover sequence length (750 frames), source in/out, title and audio tracks, visible title
pixels, restored project structure, output dimensions/duration, audible samples and representative
frames. The output's two-pixel checker must survive: a thumbnail accidentally substituted for the
source would fail. `mcp-transcript.json` retains exact requests/responses, including image blocks;
`analysis-measurements.json` retains measured wall time, payload size and worker hash/decode timings.

`failed_batch_can_be_recovered` injects an unknown second command into a three-command batch.
It requires the first edit to remain, the third not to run, accurate completed/failed counts, a fresh
state read and an undo that removes the retained edit.

`source_frames_map_fractional_vfr_reuse_speed_and_reverse` generates small ProRes MOVs with
30000/1001 fps and variable sample durations (40/80/40 ms). It checks returned presentation timestamps,
then uses the existing `TrackItem::source_time_at` for repeated use, 2× / 0.5× speed and reverse.
Subclip source offsets and half-open intervals are separately covered by engine analysis tests.

## Real desktop Bridge

Start a dedicated test instance with a new data directory and unused loopback port, then run the
same scenario. This deliberately replaces the project in that test instance; never point it at
an editor session containing unsaved work.

```sh
cargo build --release -p filmcraft -p filmcraft-cli
./target/release/filmcraft --empty --no-recover --control 52387 --data-dir /tmp/filmcraft-agent-test-data
FILMCRAFT_AGENT_TEST_BRIDGE=127.0.0.1:52387 cargo test --release -p filmcraft-cli --test agent_editing agent_editing_round_trip -- --nocapture
```

The shared scenario snapshots `doc_inspect` and `history.list` after creating the sequence, setting
a nonzero playhead and selecting a clip. A further source-analysis call must leave both snapshots
identical. This verifies project, playhead, selection and undo/redo history in the actual app;
a simulated TCP server is only a separate retry/transport test.

## Qualification record (2026-10-12)

Host: Apple M2 Max, 32 GiB RAM, macOS 27.0.1 (26A434), Rust 1.95.0. The full repository gates use release
optimization with process-local `CARGO_PROFILE_RELEASE_LTO=false` and
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`; repository release settings are unchanged. A separate
`cargo build --release -p filmcraft -p filmcraft-cli` passed with default LTO settings (2m 38s).
A default-profile test-client/desktop round trip passed in 7.23 s. After adding per-second audio
markers, the final desktop scenario passed in 8.55 s using the default desktop binary and the CI
profile test client. Final headless scenario: 7.65 s, maximum RSS 2,098,741,248 bytes for the complete
acceptance process tree (fixture generation, MCP client and export included; no compilation).

Generated artifacts are retained under `target/fixtures/agent-editing/`: `roundtrip-99472` is the
final real-desktop run and `roundtrip-99539` is the headless resource run. PIDs and timings vary on
reruns. The transcript and measurements in each directory are the source of the recorded results.

### Actual model image receipt

The current Codex conversation ran the real CLI stdio MCP server and relayed its returned image
blocks directly into the model's tool input. No local-image path was substituted for MCP image
content. Host application: ChatGPT.app 26.908.70816; bundled codex-cli 0.154.0-alpha.6.2. The session
identifies the model family as GPT-6; the exact deployment identifier is not exposed in this
execution environment and is not guessed.

Request: `media_frames {item:1, strategy:"timestamps", times_ticks:["0","1016064000000",
"2032128000000"], max_side:512}` against the generated first source video. The model observed:

| Source time | Visible image | Visible label | Deterministic expectation |
|---|---|---|---|
| 0s | red background, white circle | `1 000` | clip1, frame0 |
| 4s | green background, white triangle | `1 100` | clip1, frame100 |
| 8s | blue background, white square | `1 200` | clip1, frame200 |

All images were 512×288 and their `actual_ticks` exactly matched those source times. This validates
actual visual delivery through the stdio relay in the current Codex session. It does not claim a
blind model evaluation, human sign-off, or that persistent MCP registration was changed or tested
in the host's settings. The two registration commands above were checked against local CLI help.

### Measured source-analysis cost

Single samples, warm OS file cache, 16,152,799-byte generated MOV, 512-pixel thumbnails. Repository
CI was running concurrently. These figures are observations, not latency guarantees or cold-disk
benchmarks. Wall time includes 100 ms job polling and JSON transport. Complete JSON-RPC line bytes
include the image content and metadata copies.

| Request | Frames | Wall ms | Hash before+after ms | Decode/encode ms | JSON-RPC bytes |
|---|---:|---:|---:|---:|---:|
| First frame |1|102.54|46.33+45.87|8.61|19,175|
| Same frame, cache hit |1|102.54|45.48+45.55|0|19,131|
| Eight frames |8|324.80|45.32+45.40|62.51|136,717|
| Same eight, cache hit |8|241.43|102.46+73.75|0|136,671|

Analysis relay + MCP process tree maximum RSS: 102,416,384 bytes, measured with `/usr/bin/time -l`,
without Cargo/compiler in that process tree. The larger full-editing RSS above includes export
and must not be confused with the bounded 128 MiB analysis result cache. A cache hit still hashes
the complete file twice, so it cannot promise instant lookup. The concurrent load explains why
this is not a stable ranking of cache-hit and cold-request timings.

### Acceptance matrix

| Requirement | Status | Evidence |
|---|---|---|
| AC01 unplaced image | Pass | engine unplaced/EXIF/geometry/alpha tests; stdio PNG import and 512 px image blocks |
| AC02 uniform video sampling | Pass | real ProRes source frames, actual PTS, content_index and response budget tests |
| AC03 explicit source times | Pass | observed 0/4/8 s image blocks; fractional/VFR/edit-offset/subclip tests |
| AC04 model sees images | Pass through stdio relay | actual current Codex image receipt above; persistent native registration is not part of this recorded run |
| AC05 read-only analysis | Pass | before/after project/playhead/selection/history equality in Headless and real desktop; live-trim cancellation regression |
| AC06 budgets/hostile inputs | Pass | parameter/buffer/PTS/cycle tests; media truncation/bit-flip/valid-CRC forged-size regression added in AE08 |
| AC07 cancellation/failure | Pass | bounded queue, cancelled queued/running jobs, caught worker panic, MCP own-job cancellation and responsive parallel query |
| AC08 cache/version invalidation | Pass | full-file middle replacement preserving size/mtime, open/hash replacement race, expected-version conflict, expiry and retry receipts |
| AC09 edit/save/reopen/export | Pass | same 30 s generated-media scenario through stdio Headless and real desktop Bridge |
| AC10 original-source export | Pass |1280×720 export and two-pixel detail survive despite 512 px analysis |
| AC11 partial batch recovery | Pass | failed second step, retained first edit, skipped third, fresh state and successful undo |
| AC12 short video fallback | P1 | intentionally outside the P0 implementation |

AE06 full gate: 2,867 passed, 0 failed, 41 existing ignored. AE07 full gate: 2,870 passed, 0 failed, 41
existing ignored. Both also passed formatting, workspace Clippy, dependency layers, 69 attributed
assets and 44-crate wasm checks. AE08 also passed the complete gate: 2,872 passed, 0 failed, 41 existing ignored, plus formatting, Clippy, layers, assets and wasm. Final independent review and its fixes are recorded below.

## Final review regressions

The independent branch review found two Important issues. The bounded P0 fix rejects leading
empty edit offsets that do not exactly fit the integer video-track clock, rather than reporting
an incorrect picture or timestamp. The regression uses a 20 ms delay with a 25 Hz track clock;
two 20 ms empty edits summing to one 40 ms track unit remain supported.

Image-sequence analysis now chooses the listed source image before decoding it. An intentionally
missing slot can hold the preceding image, but corruption in that selected image is reported as a
per-request error. Tests cover an entirely failed page, a partial page retaining a valid image,
a missing slot whose selected predecessor is corrupt, and a fresh request retrying rather than
reusing a cached success. Existing missing-slot timestamp tests remain applicable.

One Minor finding remains deferred: source-bound timestamp errors are currently classified as
`INTERNAL_ERROR` at asynchronous job completion, although their message explains the invalid
range and the request fails safely. Persistent native Codex registration, blind/human model
qualification, and the separately planned P1 features are not claimed by this P0 acceptance.

Both regression tests were observed failing before their fixes and passing afterwards. The final
complete gate passed: 2,874 tests passed, 0 failed, 41 existing ignored; formatting, workspace
Clippy, dependency layers, 69 attributed assets and the 44-crate Wasm check also passed. No second
reviewer pass was used; focused RED→GREEN tests and the full gate verify the review fixes.

## P1 AP01 source preview qualification (2026-10-12)

`source_preview_video_and_png_fallback_round_trip` runs the actual CLI stdio MCP server against
original generated source media. It requests source seconds 2–6, checks eight real PNG fallback
blocks, opts the decoder test client into video, reads the controlled MP4 resource, and decodes
the result with FilmCraft. Assertions cover 160×90 output, 12 fps, exactly four seconds, unchanged
source speed/offset, red-to-green picture timing, and identical document/history snapshots.

```sh
cargo test --release -p filmcraft-cli --test agent_editing source_preview_video_and_png_fallback_round_trip -- --nocapture
```

The same test accepts `FILMCRAFT_AGENT_TEST_BRIDGE` for a dedicated desktop instance, with the same
precautions as the P0 scenario above. Generated MP4, attribution, transcript and measurements
remain under `target/fixtures/agent-editing/preview-<pid>/`.

The first headless run (`preview-17968`) passed in 4.22 seconds including fixture generation:
8-frame PNG fallback 205 ms; MP4 request/read 213 ms; MP4 22,812 bytes. These are single warm-host
observations at 160 pixels, not guarantees for 960-pixel output or cold media.

Additional engine tests cover 24000/1001 fps input with synchronized sound/picture markers,
5 fps input without upsampling, disabled audio, non-frame-aligned presentation duration,
anamorphic pixel aspect, corrupt-frame rejection, cancellation, cache expiry, active-reader
leases, strict parameters, and the 30-second input boundary. Export tests exercise the real
encoder stopping on its output budget without publishing partial data, plus exact 24 MiB writes.
Automation tests cover resource links and reads, path-like URI rejection, expired resources,
strict schemas and distinct bounded Bridge response limits.

The current Codex model also received the real `media_preview_clip` PNG fallback through a
stdio relay at 512×288. The unchanged MCP image blocks showed source 2s: red circle, label `1 050`;
source 4s: green triangle, label `1 100`; requested 5.5s / actual 5.48s: green triangle, label
`1 137`. Exact requests/responses are in `preview-17968/model-image-relay.json`.

This qualifies image fallback and MP4 encoding/resource/decoder transport. Native FilmCraft MCP
tools are still absent from this Codex session, and direct model video/audio consumption is **not
qualified**. Keep `video_capability` unknown unless that separate client/model verification has
been performed. A returned or locally decoded MP4 does not establish model video consumption.

## P1 AP02 source shot candidates

`shots_tests` covers two hard cuts, unchanged document/history, flash ambiguity, bad-frame gaps,
hostile parameters, ten-minute coverage and the 1000-candidate cap. Existing `scene_detect` and
render `scene::tests` remain regression gates. The CLI preview acceptance above also calls the
real `media_shots` MCP tool for seconds 2–6, expecting two candidates split at source second 4,
and verifies the document/history still match their pre-analysis snapshots.

## P1 AP03 source transcript and audio qualification

`transcripts_tests` checks stored words without Whisper, root-time subclip overlaps, fresh data
after a stored transcript changes, sample-exact silence spanning read blocks, nonfinite audio
rejection, hostile request parameters and ten-minute partial coverage. The CLI preview scenario
also reads a missing transcript, imports a word, queries its overlap through `media_transcript`,
and measures an actual WAV through `command_run` plus `media_analysis_result` without changing
subsequent document/history snapshots.

The default build exercises read-only queries without speech inference. This host has no speech
model installation in the FilmCraft data directory; optional model-backed recognition is **not
verified**. No weights are downloaded. With locally installed weights and speech fixtures, the
existing opt-in qualification is `cargo test -p filmcraft-speech --features whisper --test
whisper_model -- --nocapture`, using `FILMCRAFT_MODELS_DIR` when needed; a SKIPPED message is not a
recognition pass.
