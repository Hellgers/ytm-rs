# Audio continuity fix

Modified **2026-10-23**. Fork release **0.2.21**, based on upstream **v0.2.20**. This is not an official upstream release.

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
- Replacing a partially consumed source without returning its previous samples.
- Alternating track formats and end of stream through a real `Player` queue.

The validation run for the reviewed audio patch reported **69 tests passed, 5 ignored**. Ignored tests were not run; this count does not imply that all online service integrations or every output device were tested. Release packaging and repository metadata changes require their own final checks.

For a minimal independent reproduction, implement a `Source` that emits the same 55 Hz stereo samples and rate metadata in both runs. In one run return finite 2,048-sample spans; in the other expose the single finite track continuously. Feed both to rodio 0.22.2's `UniformSourceIterator` at 48,000 Hz and compare output lengths and phase after aligning their starts. Repeat with a 48,000 Hz input and output as a pass-through control. Do not make the continuous source infinite: it must end with its track.

## Limits

Removing this reproduced discontinuity does not guarantee perceptually transparent playback. A residual whistle may remain; its source has not been established by this fix. Codec artifacts, content already present in a recording, sample peaks, driver processing, network stalls, and output scheduling require separate tests. No claim is made that this patch fixes clipping or every instance of crackling.

A listening comparison should use the same encoded input, output device, normalization setting, and matched loudness. Separate decoder output from post-conversion PCM when localizing a remaining artifact. A comparison against a browser may use a different codec, stream quality, or processing path.

## References

- [Upstream v0.2.20](https://github.com/grapedjuice/ytm-rs/tree/v0.2.20).
- [rodio 0.22.2 uniform source iterator](https://docs.rs/crate/rodio/0.22.2/source/src/source/uniform.rs).
- [rodio 0.22.2 sample-rate converter](https://docs.rs/crate/rodio/0.22.2/source/src/conversions/sample_rate.rs).
- [rodio resampling issue #584](https://github.com/RustAudio/rodio/issues/584), background discussion rather than proof of every playback problem.

The original project's license and attribution remain in [LICENSE](../LICENSE) and [README.md](../README.md).
