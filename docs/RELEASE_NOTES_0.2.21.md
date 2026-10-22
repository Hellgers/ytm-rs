# ytm-rs 0.2.21 — Audio continuity fix

Modified **2026-10-23**. A release of the [Hellgers/ytm-rs fork](https://github.com/Hellgers/ytm-rs), based on upstream v0.2.20. **Not an official upstream release.**

## Audio fix

- Convert each finite decoded track continuously to the mixer's format before adding it to `Player`. AAC packet boundaries no longer restart sample-rate conversion, including the common 44,100-to-48,000 Hz path.
- Preserve frame alignment and stop with an error flag if the decoded format unexpectedly changes or ends inside a frame, without an intentional panic in the format guard.
- Rebuild the decoder and converter on seek so interpolation lookahead from the old position is discarded.
- Keep consecutive tracks independent while presenting a stable format to the playback queue.

The patch fixes a reproduced timing and phase-continuity defect. It does **not** claim to fix clipping, every whistle, or every playback interruption. A residual whistle may remain. No driver, EQ, limiter, or normalization change is included. See [the technical explanation and synthetic tests](https://github.com/Hellgers/ytm-rs/blob/v0.2.21/docs/AUDIO_CONTINUITY_FIX.md).

## Validation

The reviewed audio patch passed **69 tests**, with **5 ignored**. Synthetic regression tests cover packet boundaries, same-rate pass-through, short sources, format changes, source replacement, and alternating track formats. They require no account, copyrighted audio, or audio output device. Ignored tests and all online integrations are not covered by that result.

## Windows downloads and source

Download assets from [this release](https://github.com/Hellgers/ytm-rs/releases/tag/v0.2.21). Windows builds target x86-64 and are **unsigned**; the upstream SignPath arrangement does not cover this fork. The release includes an installer, a portable archive, complete modified source, a patch against upstream v0.2.20, and a SHA-256 manifest.

| Asset | Contents |
| --- | --- |
| `ytm-rs-setup-0.2.21.exe` | Windows x86-64 installer |
| `ytm-rs-portable-0.2.21.zip` | Portable application, license, and audio fix documentation |
| `ytm-rs-source-0.2.21.zip` | Complete modified source for this release |
| `ytm-rs-audio-continuity-0.2.21.patch` | Changes against upstream v0.2.20 |
| `SHA256SUMS.txt` | SHA-256 checksums of the release assets |

The updater uses releases from **Hellgers/ytm-rs**. You can disable automatic updates in **Settings → Updates**. For source builds, check out tag `v0.2.21` and run `cargo build --release --locked`.

Original project credits and the GPL-3.0 license are retained. The distributed source and packages exclude account credentials, application profiles, downloaded music, private logs, and diagnostic recordings.
