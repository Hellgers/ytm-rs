# ytm-rs 0.2.33 — playback cancellation and stream fallback

Based on upstream stable 0.2.31 with the continuous per-track audio fixes from
this fork's 0.2.32 release.

- Stopping, skipping, seeking and recovering an output device no longer wait for
  a network-blocked decoder on the audio control thread. Reader cancellation is
  local to each decoder, preserving a replacement using the same download.
- A stalled outgoing crossfade source cannot hold the incoming track's mixer.
- Superseded pending loads are cancelled; stale resolutions and decoder results
  cannot replace the current generation. Decoder panics report a bounded failure.
- Active HTTP, retry delays and URL renewal stop promptly when cancelled or when
  another worker fails. Failed prefetches wake every waiting worker.
- Per-block work is bounded to 30 seconds, with an eight-second no-data limit;
  rejected and stalled URLs may be renewed without mixing representations.
- Unavailable visionOS Opus responses can restart as a fresh AAC stream. Healthy
  Opus playback keeps its original format.

The earlier packet-boundary resampling fix and initial Player format priming are
retained. An output-device rebuild ends an active crossfade and resumes through a
fresh decoder at the saved position.

Windows x64 installer, portable ZIP, full corresponding source ZIP, patch against
upstream 0.2.31 and SHA256SUMS.txt are supplied. The patch now includes audio,
stream and backend source changes. Binaries are unsigned. Updates use this fork:
https://github.com/Hellgers/ytm-rs/releases

Offline regression tests cover cancellation, blocked reads, generation races,
crossfade stalls and decoder failures. Network and listening observations must
be reported separately; passing these tests alone is not listening acceptance.
