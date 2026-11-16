# Preliminary checks

Survey of 2026-11-15. [V]: verified in the source code or documentation. [S]: assumed, still to be confirmed.

## Summary of decisions

| Topic | Decision |
|---|---|
| iRacing SDK and .ibt | Format implemented in the `telemetry` crate, without an external crate |
| UI | `gpui-pre` `=0.3.12` (Zed snapshot), pinned exactly |
| ACP | `@agentclientprotocol/claude-agent-acp` 0.107.0 and `agent-client-protocol` crate 5.3.0, protocol v1 |
| Model | One adapter subprocess per role, chosen via `ANTHROPIC_MODEL`; hot switch via `session/set_config_option` (`configId: "model"`) |
| Built-in tools | `_meta.claudeCode.options.tools: []`, `allowedTools: ["mcp__coach"]`, `dontAsk` mode, and client-side denial in `session/request_permission` (second barrier) |
| MCP transport | Streamable HTTP on `127.0.0.1:<ephemeral port>` in-process (`rmcp` 3.10, `transport-streamable-http-server`), with a random Bearer token |
| Authentication | CLI subscription (claude.ai login), decided on 2026-11-15 |
| System prompt | Coach prompt injected as `override` in place of Claude Code's, decided on 2026-11-15 |
| Reference | Manual import of the Garage61 CSV; the API gives no access to laps outside one's team |
| STT/TTS | **sherpa-onnx 1.13.8** embedded in the process (decided on 2026-11-15): Parakeet v3 for STT, Piper `fr_FR-siwis` for TTS; Kyutai dropped. Measured: STT 0.2 to 0.4 s, first TTS sound 0.1 to 0.2 s on CPU |
| 1.5 s latency | **Not met cold**: Claude's first word measured at 1.78 s (Haiku, cloud); to be measured again on the PC, see the last section |

## 1. Crates for the iRacing SDK and .ibt files

Existing crates, surveyed on crates.io:

| Crate | Version / date | .ibt | Status |
|---|---|---|---|
| `kerb` | 0.4.0 / 2026-09 | no | active, `windows-sys` 0.61 |
| `pitwall` | 0.1.0 / 2025-10 | yes | single version, no public repository, requires tokio |
| `simetry` | 0.2.3 / 2024-02 | yes | multi-simulator, `windows` 0.44, no longer maintained |
| `iracing-telem` | 0.2.1 / 2023-11 | no | API almost entirely `unsafe` |
| `itelem` | 0.1.5 / 2023-04 | yes (only) | no longer maintained |
| `iracing` | 0.4.1 / 2020-12 | no | `winapi`, abandoned |

None covers both live and .ibt with active maintenance. The format is small (112-byte header, 32-byte disk sub-header, 144-byte variable descriptors) and the same parser serves both sources. It is therefore implemented in `crates/telemetry/src/irsdk.rs`. The offsets were cross-checked against pyirsdk 1.3.7, `simetry` and `kerb` [V]. I did not read the official `irsdk_defines.h` file: it is restricted to iRacing forum members.

Format points adopted:

- Live: take the buffer with the highest `tickCount`, copy the row, then check that the `tickCount` did not change during the copy (`live.rs`). Copies go through `ptr::copy_nonoverlapping`, never forming a `&[u8]` over memory modified by iRacing.
- The session YAML is in Windows-1252, or in UTF-8 if it declares `Encoding: UTF8`. Both cases are handled.
- `Lat`, `Lon` and `Alt` only exist in .ibt files. Every channel other than `SessionTime`, `Lap`, `LapDistPct` and `Speed` is therefore optional (`NaN` when missing).
- [S] Recent SDK versions reportedly use fields placed in the `pad` (`tickCountBegin` per buffer) to detect torn reads. We do not use them: reading `tickCount` twice is enough.

## 2. gpui

- [V] The `gpui` crate on crates.io is frozen at 0.2.2 (2025-10-22). Zed's README now asks for `gpui_platform`, which is not published.
- [V] `gpui-pre` is a weekly snapshot of Zed published by Longbridge, 0.3.12 of 2026-11-09. The library keeps the name `gpui`. It is also the base of `gpui-component` 0.9.1, the component library we will probably want for tables and input fields.
- [V] On Windows: Win32, DirectWrite, Direct3D 11. A release build needs `fxc.exe` (Windows SDK) or the `GPUI_FXC_PATH` variable.
- [V] On Linux, for CI: `libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libx11-xcb-dev libfreetype-dev libfontconfig-dev libwayland-dev libvulkan1`. The app was launched under Xvfb with software Vulkan (llvmpipe) and renders correctly.
- Maturity: pre-1.0, frequent breaking changes, documentation mostly in Zed's code. Versions pinned with `=`, to be updated together with `gpui-component`.

## 3. ACP: versions and compatibility

- [V] `@agentclientprotocol/claude-agent-acp` **0.107.0** (2026-11-13), on `@agentclientprotocol/sdk` 2.6.0 and `@anthropic-ai/claude-agent-sdk` 0.3.323. The old `@zed-industries/claude-code-acp` package has been deprecated since 0.16.2.
- [V] The adapter answers `protocolVersion: 1`; it can also speak the v2 draft if the client asks for it.
- [V] Standalone Windows executables bundle the adapter and `claude.exe`, without Node. This is the simplest way to distribute it.
- [V] `agent-client-protocol` crate **5.3.0** (2026-11-14), schema `=1.18.0`, `ProtocolVersion::LATEST = V1`: **compatible**.
- [V] The Rust API has changed since the common examples: `ClientSideConnection` and the `Client` trait to implement no longer exist. The client is built from handlers, for example `Client.v1().on_receive_notification(...).on_receive_request(...).connect_with(AcpAgent::from_str(cmd)?, ...)`. It needs the `process` feature, plus `unstable_session_options` for `configOptions`. Handlers and futures must be `Send`, without `LocalSet`. The engine does not impose a runtime.

## 4. Model selection and tool restriction

Model [V] (`dist/session-model.js`):

- When the session is created, the priority order is: `ANTHROPIC_MODEL`, then `model` in settings.json, then the model of a resumed session, then the first available model.
- During a session, the model is changed with `session/set_config_option` and `configId: "model"`. There is no `session/set_model` handler.
- `configOptions` in `session/new` also works, but the feature is marked unstable.
- Choice: one adapter subprocess per role (on-track conversation, analysis), each with its own `ANTHROPIC_MODEL`. The two sessions are isolated and the model stays configurable per role.

Tools [V] (`dist/acp-agent.js`):

- `_meta.claudeCode.options.tools: []` removes all built-in tools.
- `allowedTools: ["mcp__coach"]` pre-authorizes our whole server. MCP tools are exposed as `mcp__<server>__<tool>`.
- The `dontAsk` mode (in `permissions.defaultMode` or via `configId: "mode"`) denies anything not pre-authorized.
- The adapter only sends `session/request_permission` when Claude Code's rules yield "ask". Our handler still approves `mcp__coach__*` and denies the rest, as a second barrier.
- `promptSuggestions: false`, otherwise a follow-up suggestion is generated on every turn.

System prompt and CLAUDE.md [V]:

- By default, `settingSources` is `["user","project","local"]`: the CLAUDE.md of the working directory is loaded, but so are the machine's user settings.
- The ACP field `systemPrompt: {mode: "override", content: [...]}` fully replaces Claude Code's prompt.
- **Decision (2026-11-15)**: the coach's `CLAUDE.md` file stays in the sessions directory, but the program reads it and injects it as `override`, with `settingSources: []`. This removes Claude Code's system prompt (about 15 to 25 k tokens with the tools [S]) and the machine's personal settings. It is the main latency lever. Deliberate departure from the specification, where Claude Code loaded the CLAUDE.md itself.
- [S] To check at milestone 4: `settingSources: []` must not cut off access to the login. Credentials are stored separately from the settings, which should be enough.

## 5. MCP transport

- [V] The adapter advertises `mcpCapabilities: {http: true, sse: true}`. stdio is always accepted. MCP over ACP is not advertised.
- [V] `rmcp` 3.10.0 provides `StreamableHttpService` (`transport-streamable-http-server` feature), with `allowed_hosts` restricted to loopback by default.
- Choice: in-process HTTP server on `127.0.0.1` with an ephemeral port, declared in `session/new` as `{type: "http", name: "coach", url, headers: [{name: "Authorization", value: "Bearer <random token>"}]}`. stdio would require a separate executable, which would then have to reach the main program's state.
- The `agent-client-protocol-rmcp` 5.3.0 crate (on rmcp ^3.4) exists, to be evaluated at milestone 4.

## 6. Claude Code authentication

- [V] The adapter uses the credentials of the `claude` CLI: either an existing claude.ai login, or `ANTHROPIC_API_KEY`, `apiKeyHelper` or a Console key.
- **Decision (2026-11-15): CLI subscription**, for personal use. No API key.
- Setup: log in once on the machine with the CLI (`claude`, then `/login`).
- The program removes `ANTHROPIC_API_KEY` from the environment of the adapter subprocess. Otherwise the CLI would use the key instead of the subscription, and bill it.
- Do not pass `--hide-claude-auth` to the adapter: that option refuses precisely the sessions paid for by subscription.
- For the record, the Agent SDK documentation does not allow distributed third-party products to offer claude.ai login. If the application were ever distributed, it would have to go back to an API key.
- The subscription's usage limits apply: an Opus analysis session running on every lap will quickly use up the quota. To watch at milestone 6.

## 7. Garage61 CSV

- Likely columns, based on projects that parse these exports: `Speed, LapDistPct, Lat, Lon, Brake, Throttle, RPM, SteeringWheelAngle, Gear, Clutch, ABSActive, DRSActive, LatAccel, LongAccel, VertAccel, Yaw, YawRate, PositionType`. [S]: not checked on a real export, garage61.net was not reachable from this session. `YawRate` is the least certain column.
- Missing: `SessionTime`, `LapDist` and the lap number. Time is derived from the index (60 Hz); alignment is done on `LapDistPct`, which is present. The last few percent of the lap are sometimes missing.
- If `YawRate` is missing but `Yaw` is present: differentiate the unwrapped `Yaw` over time.
- If both `YawRate` and `LatAccel` are missing:
  1. Project Lat/Lon to local meters.
  2. Resample along the arc length.
  3. Smooth (Savitzky-Golay; the resolution of Lat/Lon as float32 is about 0.4 m).
  4. Compute the curvature κ, then `YawRate ≈ v·κ` and `LatAccel ≈ v²·κ`.

  This estimate ignores slip, camber and gravity: it is good enough to compare lines, not to measure fine oversteer.
- Corner detection in the `telemetry` crate already uses `YawRate / Speed`, falling back to `LatAccel / Speed²`. Both paths are tested.

## 8. Garage61 API

- [V, via the documentation copied into the `garage61api` wrapper] Base `https://garage61.net/api/v1`, `Authorization: Bearer <token>` (personal token or OAuth2 PKCE, `driving_data` scope). Endpoints: `GET /laps` (filters on car, track, drivers, teams) and `GET /laps/{id}/csv`.
- [V] "This method only returns laps driven yourself or your teammates… For privacy reasons, no global lap search is available." There is **no API access to laps of drivers outside one's team**.
- [S] Data packs published by teams have a dedicated endpoint (`/teams/{team}/datapacks/...`). Whether a plain subscriber can download them through the API is not verified.
- The garage61.net developer page is a JavaScript application: its documentation cannot be read without a browser, and so could not be rechecked from the session.
- Decision: **manual CSV import** at milestone 2. API import of one's own laps and one's team's will come later as an option, with the token in `GARAGE61_TOKEN`.

## 9. STT and TTS: sherpa-onnx (decision of 2026-11-15)

Kyutai (moshi-server) is ruled out: no official Windows support, a TTS module with embedded Python, CUDA required and a separate server. The notes on its protocol are in the git history of this file.

Chosen: **sherpa-onnx**, embedded in the process, with no server, no Python and no WSL.

### Version: 1.13.8, not 2.x

- [V] Since 2.0, sherpa-onnx no longer bundles the espeak-ng phonemizer for Piper/VITS voices: `--vits-data-dir` is ignored, and synthesis fails for lack of `phoneme_codepoints` supplied by an external phonemizer. Tested with 2.0.1.
- **Decision: pin the `sherpa-onnx` crate at `=1.13.8`**, the last 1.x version, which still phonemizes on its own. Its Rust API is the same as 2.0.1 for our use.
- Moving to 2.x later will require an external phonemizer. Candidates: the `espeak-ng` crate (pure Rust port, 0.2.0) or bindings to the C library.
- License: espeak-ng is GPL-3, and it is linked into the sherpa-onnx 1.x library. No problem for personal use; to be reviewed if the application were ever distributed.

### Library distribution

- [V] The `sherpa-onnx-sys` build script downloads a prebuilt library from the GitHub releases (static by default). On a machine with normal Internet access, `cargo build` is enough.
- [V] The same libraries are published on npm (`sherpa-onnx-linux-x64`, `sherpa-onnx-win-x64`) and on PyPI (`sherpa-onnx-core`). To use them: the `shared` feature, plus `SHERPA_ONNX_LIB_DIR` pointing to the library folder. This is what was used for the measurements below, since the GitHub releases were unreachable from the cloud session.

### French models (verified on Hugging Face, `csukuangfj/...` repositories)

| Use | Model | Size | License |
|---|---|---|---|
| STT | `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` (25 European languages) | ~670 MB | CC-BY 4.0 (NVIDIA) [S] |
| STT, alternative | `sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8` | smaller | [S] |
| TTS | `vits-piper-fr_FR-siwis-medium` (female voice) | 63 MB | CC-BY 4.0 [V] |
| TTS | `vits-piper-fr_FR-tom-medium` (male voice) | 61 MB | **AGPLv3** [V]: avoid |
| TTS | other Piper `fr_FR` voices: gilles, mls, upmc, tjiho, miro | | to check one by one |
| TTS | multilingual Kokoro, voice `ff_siwis` (speaker 30) | ~300 MB [S] | Apache 2.0 [S] |

### Measurements (cloud session, 4 shared vCPUs, no GPU, 4 threads)

| Step | Measurement |
|---|---|
| Loading Piper TTS / Parakeet STT | 1.0 s / 2.1 s, once at startup |
| TTS: first audio chunk, 2 to 3.4 s sentence | 80 to 180 ms (about 20 times faster than real time) |
| STT: 2 to 3.4 s sentence | 210 to 360 ms |
| STT: real 5 s recording (`test_wavs/fr.wav`) | 415 ms, exact transcription |

French coaching sentences synthesized then transcribed back (English glosses in parentheses):

- « Freine cinq mètres plus tard au T5. » → « Freine 5 mètres plus tard au T5. » (Brake five meters later at T5.)
- « Tu perds deux dixièmes au virage quatre, reprends les gaz plus tôt. » → « Tu perds deux dixièmes au virage 4. Reprends les gaz plus tôt. » (You're losing two tenths in corner four, get on the throttle earlier.)
- « Je n'ai plus de rotation au T5, qu'est-ce que je change ? » → identical. (I've lost rotation at T5, what should I change?)

Parakeet writes numbers as digits, which makes it easier to identify corners ("T5", "virage 4"). A recent desktop PC should do better than these shared vCPUs.

### Planned use

- Push-to-talk STT: audio is accumulated while the button is held and transcribed in one go on release.
- TTS sentence by sentence, as `agent_message_chunk` events arrive.
- The `voice` crate will expose `Stt` and `Tts` traits so other engines can be plugged in: Windows SAPI/WinRT voices as a fallback, Kokoro for comparison.
- Audio capture and playback: `cpal` (WASAPI on Windows).
- Models are downloaded on first launch from Hugging Face into the user's data directory, and are not versioned.
- Downloads use the system certificate store (`ureq` with `platform-verifier`) rather than a bundled list: they work behind a corporate proxy that inspects TLS. Verified from the cloud session, which goes through such a proxy.
- Windows: the static library is compiled with the static C runtime (`/MT`); `.cargo/config.toml` enables `+crt-static` for the MSVC target. Verified: Windows CI compiles, links and passes the tests with the static library (run of 2026-11-15, commit 08e9af8).

## 10. Input: background push-to-talk

- [V] `gilrs` 0.11 uses Windows Gaming Input by default, which "requires an in focus window to be associated with the process to receive events" (gilrs README). Unusable while iRacing has focus. Its XInput backend works in the background but only sees Xbox controllers, not wheels.
- Chosen on Windows: **DirectInput 8**, cooperative level `DISCL_BACKGROUND | DISCL_NONEXCLUSIVE`, on a hidden top-level window. Class `DI8DEVCLASS_GAMECTRL` (wheels, pedals, button boxes, joysticks, gamepads). The `windows` crate does not expose the predefined `c_dfDIJoystick2` format: custom format with 128 buttons and 4 hats, all optional.
- Keyboard and mouse: `device_query` (`GetAsyncKeyState` on Windows, X11 on Linux), global reading independent of focus.
- Linux (development): `gilrs` (evdev, works in the background).
- Verified: push-to-talk capture and tracking work with the real keyboard (X11, under Xvfb + xdotool); the DirectInput code compiles and passes clippy for the Windows target. **Not verified** on a real wheel or button box: to be tested on the PC.

## 11. Planning (milestone 6)

- Analysis model: `ANTHROPIC_MODEL=sonnet` for the analysis subprocess, thinking not disabled (no latency constraint: the analysis runs in the background).
- Two adapter subprocesses (conversation, analysis), each with its own MCP endpoint. Each role's model stays configurable (`--model`, `--analysis-model`).
- Not measured with Claude: actual duration of an analysis and quota consumption per lap. Verified with a mock agent: triggering, real `update_plan` call through the MCP server, coach layer applied, driver plan untouched.

## 12. Conversation latency

### Actual measurement (2026-11-15)

Measured with the `probe` example of the `coach` crate, from the cloud session, adapter 0.107.0, Haiku, thinking disabled, coach prompt as `override`, no tools:

| Step | Measurement |
|---|---|
| Adapter startup until the session is ready (`initialize` + `session/new`) | 2.6 s, paid once at launch |
| Message sent → first `agent_message_chunk` | **1.78 s** |
| Message sent → end of turn | 2.13 s |

A single cold measurement (first message of the session, empty prompt cache), from a cloud container: to be repeated on the driver's PC, where the network and cache differ. The reply followed the coach prompt (two sentences, in French, no formatting).

Note: this measurement was not planned. The adapter had been started with an empty environment to check only `initialize` and `session/new` without credentials; the cloud session's network infrastructure authenticated the call on its own. No other real call was made.

### Budget

| Step | Estimate or measurement |
|---|---|
| STT: from button release to final text | **measured**: 0.2 to 0.4 s (Parakeet v3 int8, CPU) |
| Claude: first word | **measured**: 1.8 s cold from the cloud; 1 to 1.5 s expected with a warm cache |
| TTS: from the first sentence to the first sound | **measured**: 0.08 to 0.18 s (Piper, CPU), plus audio output latency |
| **Total** | **2.1 to 2.4 s cold**; 1.4 to 2 s expected with a warm cache |

**The 1.5 s target is not met** on the only measurement available; Claude accounts for most of the delay. Levers, from simplest to heaviest:

- the coach's short acknowledgement ("Looking at T5.") is spoken as soon as the first chunk arrives: it hides the duration of tool calls, not that of the first word;
- keep the session warm and the system prompt stable to benefit from the prompt cache (first message slower than the following ones);
- play a pre-recorded local acknowledgement as soon as speech ends ("Copy."), without waiting for Claude;
- as a last resort, call the Messages API directly for the conversation, keeping ACP for the analysis.

The interface shows the first-word latency on every turn; the measurement on the driver's PC will decide.
