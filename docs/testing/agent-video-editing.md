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

## Qualification record (2026-10-12, in progress)

Headless round trip, failure recovery and fractional/VFR/time-mapping tests passed (three tests).
The first complete scenario took 6.02 seconds for all three tests on this host with release
optimization and process-local LTO disabled. This is test-suite elapsed time, not a promised export
or inference latency. The process-local CI profile override does not change repository release
settings. The default-profile production build and real desktop scenario are recorded below when
completed; no pending check is counted as passed.

Default production release build passed with repository LTO settings (`cargo build --release -p
filmcraft -p filmcraft-cli`, 2m38s). A separate real desktop instance on loopback port52387, isolated
`desktop-data`, passed the entire round trip in7.23s using the default release test build. The saved
record is `target/fixtures/agent-editing/roundtrip-99299/`. That timing precedes the later per-second
audio marker assertion; subsequent qualification uses the final fixture definition above.

The final desktop scenario, including the strict460Hz audio marker assertion, passed in8.55s; artifacts: `target/fixtures/agent-editing/roundtrip-99472/`. The desktop binary uses the default release profile; this last test-client build uses the process-local CI profile override. All planned scenario assertions passed, including unchanged `doc_inspect` and `history.list` snapshots.
