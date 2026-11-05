# Audio continuity fix

Updated **2026-11-05** for upstream **v0.2.31** (`133ad8990a6cd60eafdc812b333ef356b52380f7`). The original fix was reviewed for fork **0.2.21**, based on upstream **v0.2.20**, on **2026-10-23**. This is a fork-specific patch, not an official upstream release.

## Reproduced defect

The pinned audio stack uses rodio **0.22.2**, cpal **0.17.3**, and Symphonia **0.5.5**. AAC decoding exposes finite PCM spans, commonly 1,024 stereo frames (2,048 interleaved samples) per packet. A packet boundary does not necessarily mean that the sample rate or channel count changed.

In rodio 0.22.2, `UniformSourceIterator` creates a new `SampleRateConverter` for each finite span. The converter contains interpolation phase and lookahead frames. Treating each decoded packet as an independent conversion restarts that state: interpolation cannot carry across the boundary, and output frame counts can accumulate rounding differences. For example, converting a packetized 44,100 Hz source to a 48,000 Hz mixer can introduce timing drift and periodic phase modulation. This is distinct from an HTTP buffer underrun or changing an output device's maximum advertised sample rate.

An offline synthetic test with a 55 Hz stereo sine wave at amplitude 0.1 reproduces the difference without a sound device. With continuous conversion, ten seconds at 44,100 Hz produces 960,000 interleaved samples at 48,000 Hz. Restarting conversion at 2,048-sample spans produces 960,382 in that test. A separate test through `Player`, a private mixer, and an output mixer reproduces the modulation. These results establish an algorithmic defect; they do not establish that every audible artifact has this cause.

## Change

`src/audio_source.rs` wraps each **finite decoded track**, checks its input format, and converts it to the private mixer's rate and channel count **before** appending it to `Player`:

```text
decoded packets -> complete-frame format guard -> continuous per-track converter
                -> Player controls and queue -> private mixer -> output mixer
```

The frame wrapper exposes a continuous span while that particular track's format remains constant. It buffers a complete input frame before returning any of its samples. If the decoder reports a different sample rate or channel count, or ends inside a frame, it ends the source and sets an error flag. The playback engine reports that failure without an intentional panic in the format guard. It does not silently reinterpret samples under stale metadata.

Each new track gets a separate converter and a natural end of stream. The queue therefore receives a stable mixer format even when consecutive tracks have different input formats. Merely hiding spans on the whole `Player` queue would be unsafe: its keep-alive silence could conceal the next track's format change indefinitely.

The v0.2.31 integration covers both variants of `TrackDecoder`: AAC and Opus. Conversion happens before `Meter`, so the crossfade ramps count samples in the fixed private mixer format. Each outgoing player retains its own converter while the incoming track gets a new one. The private mixer's rate and channels are chosen once in `Output::new` and retained when `Output::open` recovers or changes a device. The existing `Bridge` returns that same mixer on a frame boundary and lets the device mixer adapt to the new device's format. Its continuous span describes this fixed-format mixer; it does not hide a track-changing `Player` queue.

## Empty-player metadata at startup

The updated regression tests also exposed a separate rodio 0.22.2 queue bootstrap defect. `Player::connect_new` calls `Mixer::add` while the queue is empty and reports its default mono / 48 kHz format. The mixer's converter captures those values immediately. If a stereo source is appended before that queue is first consumed, its first 512 interleaved samples are interpreted as mono and duplicated. A synthetic 100 ms stereo source at 48 kHz consequently produces 10,112 nonzero output samples instead of 9,600. The subsequent mono 44.1 kHz track after idle silence produces the expected 9,600; this is an initial metadata problem, not a repeated EOF tail or the earlier packet-phase defect.

`audio_source::connect_player` now creates the queue explicitly, appends a zero-length source in the fixed private mixer format, and synchronously consumes one complete target-format silence frame before attaching the queue to the mixer. This retires the metadata seed and establishes correctly formatted keepalive silence without counting a pending sound. Synchronous retirement matters: leaving even a zero-length seed pending could make `Player::clear` wait for a disconnected output device. Consuming a complete target frame also preserves six-channel alignment. The original queue continues to expose its own span boundaries. Startup and both crossfade player-creation paths use this helper.

## Playback stalls investigated on 2026-11-06

The command thread must not wait for decoder input or for a device to consume a source. rodio's `Player::clear` waits for source completion, even if its PCM is already fully buffered. Calling it before cancelling an old streaming reader therefore blocks further audio commands while the output callback waits for network data. Without an output consumer the wait is indefinite. Device recovery has a related constraint: WASAPI joins its callback thread when the old device is dropped, so pending reader waits must be interrupted before that join. These are separate from the packet-phase and initial queue-metadata defects above.

Player replacement now stops and drops the old controls without `clear`, preserving gain and pause state on a fresh primed player. A reader-local interrupt wakes the retired decoder without cancelling another decoder's shared download. Seek and output recovery reserve the replacement reader before interrupting the old one, then build the fresh decoder off the command thread. Pending rebuilds suppress normal EOF/early-next events; both success and failure pass through the current generation and seek-sequence gate. Stopping, a new track, or a newer seek cancels superseded builds. Output recovery retires active and fading reader waits before dropping the old device, and reconstructs the active decoder at its last position. A device change ends an ongoing crossfade and may discard buffered audio around the recovery point.

Stop and outgoing-fade retirement cancel their own reader reservations, with whole-download cancellation left to the last-reader lifetime policy. A replacement using the same `Shared` may already have been reserved before an older Stop command arrives. The decoder worker catches unwinds as a typed, sequence-gated failure with a fixed message, and releases its busy state so later jobs can run. An outgoing crossfade source waiting for network input is retired immediately when starting/handoff detects it, or on the next 100 ms engine poll, including while the incoming player is paused. This prevents one stalled old source from blocking the mixer's ready incoming source.

The per-track converter is now lazy: its constructor and `Player::append` only publish target-format metadata. rodio's initial resampling lookahead runs on the first source pull, where reader-local cancellation can release it, rather than blocking the command thread during source construction. The complete-frame guard and continuous per-track resampling remain in place.

Synthetic tests reproduce `clear` waiting without an output consumer and release that intentional reproduction before joining its thread. Separate checks verify nonblocking replacement with gain/pause preservation, zero decoder reads during conversion/append construction, and recovery preparation interrupting old readers while retaining a fresh reservation. These establish code-level cancellation and ordering behavior; they do not claim physical-device or listening acceptance.

Additional bounded tests exercise a typed decoder panic followed by another scheduled job, and a real missing-block `StreamReader` inside an outgoing mixer source. Retiring that waiting source must unblock the ready incoming PCM with its exact sample count, without an audio device or network request.

Seeking builds a fresh decoder and converter through the existing replacement path. Old interpolation lookahead and phase are discarded. Delegating a seek through rodio's existing converter would leave its buffered interpolation state intact, so the new wrapper intentionally does not provide in-place seeking.

This patch retains the pinned converter's interpolation method. It does not add a new high-quality resampling library, equalizer, limiter, normalization policy, driver, or virtual audio device.

## Synthetic regression tests

No copyrighted recording or signed-in account is required. Build the tagged source and run:

```sh
cargo test --locked
cargo test --locked audio_source::tests
```

The tests construct synthetic samples in memory and do not open an audio output device. Coverage includes:

- A packetized 44,100 Hz stereo tone converted to 48,000 Hz, compared sample-for-sample with a single continuous conversion, including output length.
- Same-rate pass-through, empty sources, and very short complete frames.
- A format change terminating without a panic or partial output frame.
- A channel-count change dropping the entire affected frame, and a truncated final frame setting the failure flag at equal and different conversion rates.
- Replacing a partially consumed source without returning its previous samples.
- Alternating track formats and end of stream through a real `Player` queue.
- A new track with a different format arriving after the player's keepalive silence, retaining the expected output sample count.
- Initial queue priming that leaves `Player::empty` true and allows `clear` without consuming output, plus exact asymmetric channel patterns at 1/2/6 channels and 44.1/48/96 kHz at startup and after idle silence.
- Crossfading converted 48 kHz stereo and 44.1 kHz mono sources in one mixer format, plus frame-aligned handback of the fixed-format mixer during simulated output recovery.

The historical validation run for the original reviewed audio patch reported **69 tests passed, 5 ignored**. That count does not describe the updated v0.2.31 integration. Its checks must be rerun on the final merged tree. Ignored tests were not run; synthetic tests do not establish online service, physical-device recovery, or listening acceptance. Release packaging and repository metadata changes require their own final checks.

For a minimal independent reproduction, implement a `Source` that emits the same 55 Hz stereo samples and rate metadata in both runs. In one run return finite 2,048-sample spans; in the other expose the single finite track continuously. Feed both to rodio 0.22.2's `UniformSourceIterator` at 48,000 Hz and compare output lengths and phase after aligning their starts. Repeat with a 48,000 Hz input and output as a pass-through control. Do not make the continuous source infinite: it must end with its track.

## Limits

Removing this reproduced discontinuity does not guarantee perceptually transparent playback. A residual whistle may remain; its source has not been established by this fix. Codec artifacts, content already present in a recording, sample peaks, driver processing, network stalls, and output scheduling require separate tests. No claim is made that this patch fixes clipping or every instance of crackling.

A listening comparison should use the same encoded input, output device, normalization setting, and matched loudness. Separate decoder output from post-conversion PCM when localizing a remaining artifact. A comparison against a browser may use a different codec, stream quality, or processing path.

## References

- [Upstream v0.2.20](https://github.com/grapedjuice/ytm-rs/tree/v0.2.20).
- [Upstream v0.2.31](https://github.com/grapedjuice/ytm-rs/tree/v0.2.31).
- [rodio 0.22.2 uniform source iterator](https://docs.rs/crate/rodio/0.22.2/source/src/source/uniform.rs).
- [rodio 0.22.2 sample-rate converter](https://docs.rs/crate/rodio/0.22.2/source/src/conversions/sample_rate.rs).
- [rodio resampling issue #584](https://github.com/RustAudio/rodio/issues/584), background discussion rather than proof of every playback problem.

The original project's license and attribution remain in [LICENSE](../LICENSE) and [README.md](../README.md).
