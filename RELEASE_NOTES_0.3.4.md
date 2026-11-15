# ytm-rs 0.3.4 - Playback, radio and rendering fixes

Windows x64 fork release based on upstream ytm-rs 0.3.3
(`7fe50ee97213771e56385973369d2680d6b972ca`). This combines the playback
continuity and recovery fixes with seed-based radio and rendering improvements,
while retaining upstream video playback, HLS, RSS feeds, speed control,
normalization, equalizer, limiter, library and Jam features.

## Changes

- Keep resampling continuous across finite decoder spans and prime the mixer queue
  in its actual format. The reproduced defect concerns conversion phase and
  output length; it does not establish that AAC decoding itself is broken.
- Defer decoder/DSP reads off the audio command thread, cancel superseded work,
  resume interrupted downloads without shifting cached bytes and bound playback
  network waits. Recovery verifies decoder output at the requested position and
  retains the first complete PCM frame.
- Keep the current track and exact position when recovery fails. Bound automatic
  retries, reset their budget after real progress and support manual retry from
  the saved position.
- Follow YouTube Music's seed-specific automix session and continuations. Keep
  server order, include recommended music videos, exclude explicit podcasts and
  use bounded account-specific in-memory history to prefer novel candidates.
  Finite recommendation pools may return fewer tracks; no random filler is used.
- Reuse shaped lyric words with font/atlas and DPI invalidation, preserving
  upstream typography and translation-invariant glow caching. Evaluate animated
  background noise on a vertex grid while retaining theme and video controls.
- Include generated AAC/Opus fixtures and deterministic regressions for audio,
  HTTP recovery, target preparation, radio parsing and rendering caches.

## Downloads and source

The Windows installer and portable executable are **unsigned**. Check the
SHA-256 hashes against `SHA256SUMS.txt` before running either download.

- `ytm-rs-setup-0.3.4.exe`: Windows x64 installer.
- `ytm-rs-portable-0.3.4.zip`: portable executable, marker, license and documentation.
- `ytm-rs-source-0.3.4.zip`: complete corresponding source for the release commit.
- `ytm-rs-unified-fixes-0.3.4.patch`: binary-capable unified functional patch
  against upstream `7fe50ee97213771e56385973369d2680d6b972ca`; excludes fork-only
  updater/release infrastructure and branding. Apply it at that base with
  `git apply --binary ytm-rs-unified-fixes-0.3.4.patch`.
- `SHA256SUMS.txt`: hashes of the four files above.

Existing license and attribution are retained. Test audio is synthetic; private
recordings, account data and credentials are not release inputs. The complete
source archive includes fork packaging, while the functional patch is suitable
for reviewing the application fixes separately.

## Validation

The release workflow runs release-profile tests, strict Clippy, build and privacy
scans before attaching candidate assets. Exact validation results, workflow links
and native checks are recorded on the corresponding GitHub release page.
Ignored network tests require separate execution. Headless tests and pattern
scans do not establish all-account, physical-device, visual or human acceptance.
No CPU/GPU percentage from an older version is asserted for this integration.

The fork workflow attaches assets to a draft for final verification before
publication. These notes and the fork packaging workflow are separate from the
functional upstream PR.
