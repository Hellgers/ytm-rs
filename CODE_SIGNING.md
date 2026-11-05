# Code signing policy

## This fork

The **0.2.32** Windows binaries published in [Hellgers/ytm-rs releases](https://github.com/Hellgers/ytm-rs/releases) are **unsigned**. They incorporate upstream **0.2.31** and the audio continuity patch, updated on **2026-11-05**. They are not official upstream releases. No SignPath signature or certificate is claimed for these binaries. The earlier fork 0.2.21 binaries were unsigned too.

The release supplies the complete modified source, a patch against upstream v0.2.31, and SHA-256 checksums for its assets. A matching checksum verifies that a download matches that manifest; it is not a code-signing certificate or an independent security review. You can build the tagged source using the instructions in [README.md](README.md).

## Upstream signing policy

The following signing arrangement belongs to the original [grapedjuice/ytm-rs](https://github.com/grapedjuice/ytm-rs) project. It does not transfer to this fork. Upstream documents that its Windows releases are built by GitHub Actions rather than on a personal machine.

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

### Upstream team

- Committers and reviewers: [grapedjuice](https://github.com/grapedjuice)
- Approvers: [grapedjuice](https://github.com/grapedjuice)

## Privacy

The audio continuity patch operates locally on decoded samples. Release source and packages must not include account credentials, application profiles, downloaded music, diagnostic recordings, or private logs. The service descriptions below are retained from upstream; they are not an exhaustive network audit.

ytm-rs only talks to the services it needs to play your music:

- **YouTube Music and Google** (`music.youtube.com`, `youtube.com`, `googlevideo.com`, `accounts.google.com`) for browsing, playback, likes, playlists and sign-in. Your sign-in cookie stays on your PC.
- **Better Lyrics** (`lyrics.api.dacubeking.com`) receives the song's YouTube ID, title, artist, album and length to find synced lyrics.
- **GitHub** (`github.com`) to download `yt-dlp` when an age-restricted song is played on an account without Premium, and to refresh it weekly after that.
- **Discord**, through the Discord app on your PC, when the "Show what I'm playing on Discord" setting is on.

It sends no analytics or telemetry, and nothing goes to the developer.
