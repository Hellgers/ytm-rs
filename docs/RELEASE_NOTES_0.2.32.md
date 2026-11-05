# ytm-rs 0.2.32 — upstream 0.2.31 with audio continuity

This Windows x64 fork release incorporates upstream stable **0.2.31**
(`133ad8990a6cd60eafdc812b333ef356b52380f7`) and retains the per-track
resampling fix from this fork's 0.2.21 release.

- Includes the original 0.2.31 features and fixes, including account channel
  switching, Opus playback, crossfade, output recovery, and updated themes.
- Converts each finite track to the private mixer's stable format before Player,
  keeping resampling phase across decoder packet boundaries.
- Creates a fresh decoder and converter on seek, avoiding stale interpolation
  samples. Queue EOF, crossfade, and output recovery remain separate from the
  finite track converter.
- Automatic updates, release notes, and download statistics use **Hellgers/ytm-rs**.
  The existing update setting remains enabled by default: checks occur after
  startup and every six hours, with installation on normal app exit or when
  choosing Restart to update.

Assets: Windows x64 installer, portable ZIP, full corresponding source ZIP,
audio patch against upstream 0.2.31, and SHA256SUMS.txt. Binaries are unsigned.
The fork does not publish macOS, Linux, or ARM64 binaries in this release.

The audio regression tests use synthetic samples and real rodio queues and
mixers without opening hardware. They do not replace a listening check on a
particular device. Historical acceptance of the earlier patch does not establish
listening acceptance of this new build. No private recordings or account data
are included.

Original project: https://github.com/grapedjuice/ytm-rs
Fork releases: https://github.com/Hellgers/ytm-rs/releases
