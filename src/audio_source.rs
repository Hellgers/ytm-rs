//! Convert each finite track before it enters Player's never-ending queue.
//! Packet boundaries do not imply a PCM format change and must not reset phase.
use rodio::{ChannelCount, SampleRate, Source, source::UniformSourceIterator};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Connect an empty player whose queue already has the private mixer's format.
/// rodio's unprimed empty queue reports mono / 48 kHz. Mixer::add captures that
/// metadata immediately, before the first real source can supply its format.
pub fn connect_player(
    mixer: &rodio::mixer::Mixer,
    channels: ChannelCount,
    rate: SampleRate,
) -> rodio::Player {
    let (player, mut queue) = rodio::Player::new();
    player.append(rodio::source::Zero::new_samples(channels, rate, 0));
    // Retire the zero-length metadata seed synchronously: leaving it queued
    // would make Player::clear wait for an output device that may be absent.
    // Enter the correctly formatted keepalive silence and consume one complete
    // frame so a multichannel track will also start on a channel boundary.
    for _ in 0..channels.get() {
        let _ = queue.next();
    }
    // Preserve the queue's own span boundaries, including its finite silence.
    // Never replace them with an unconditional continuous span.
    mixer.add(queue);
    player
}

pub fn convert<S>(
    inner: S,
    channels: ChannelCount,
    rate: SampleRate,
    failed: Arc<AtomicBool>,
) -> impl Source + Send
where
    S: Source + Send,
{
    let input_channels = inner.channels();
    let input_rate = inner.sample_rate();
    UniformSourceIterator::new(
        Frames {
            inner,
            channels: input_channels,
            rate: input_rate,
            frame: vec![0.0; input_channels.get() as usize],
            at: input_channels.get() as usize,
            ended: false,
            failed,
        },
        channels,
        rate,
    )
}

struct Frames<S> {
    inner: S,
    channels: ChannelCount,
    rate: SampleRate,
    frame: Vec<f32>,
    at: usize,
    ended: bool,
    failed: Arc<AtomicBool>,
}

impl<S: Source> Iterator for Frames<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.ended {
            return None;
        }
        if self.at == self.frame.len() {
            for i in 0..self.frame.len() {
                let Some(sample) = self.inner.next() else {
                    self.ended = true;
                    // A truncated frame must never shift the channel alignment.
                    if i != 0 {
                        self.failed.store(true, Ordering::Release);
                    }
                    return None;
                };
                // Symphonia updates its format while next() loads a new packet.
                // Check after next(), before any sample of that frame is exposed.
                if self.inner.channels() != self.channels || self.inner.sample_rate() != self.rate {
                    self.failed.store(true, Ordering::Release);
                    self.ended = true;
                    return None;
                }
                self.frame[i] = sample;
            }
            self.at = 0;
        }
        let sample = self.frame[self.at];
        self.at += 1;
        Some(sample)
    }
}

impl<S: Source> Source for Frames<S> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        self.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    // Seeking is intentionally unsupported here. The engine builds a fresh
    // decoder and converter for every seek, so old interpolation state is gone.
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;
    fn nz(n: u32) -> SampleRate {
        SampleRate::new(n).unwrap()
    }
    fn ch(n: u16) -> ChannelCount {
        ChannelCount::new(n).unwrap()
    }
    fn flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    struct Packet {
        data: Vec<f32>,
        n: usize,
        rate: SampleRate,
        channels: ChannelCount,
        change: Option<usize>,
        channel_change: Option<usize>,
    }
    impl Iterator for Packet {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            let x = self.data.get(self.n).copied()?;
            self.n += 1;
            Some(x)
        }
    }
    impl Source for Packet {
        fn current_span_len(&self) -> Option<usize> {
            Some(2048 - self.n % 2048)
        }
        fn sample_rate(&self) -> SampleRate {
            if self.change.is_some_and(|n| self.n >= n) {
                nz(32000)
            } else {
                self.rate
            }
        }
        fn channels(&self) -> ChannelCount {
            if self.channel_change.is_some_and(|n| self.n >= n) {
                ch(1)
            } else {
                self.channels
            }
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }
    #[test]
    fn packet_boundaries_do_not_reset_phase_or_add_frames() {
        let data: Vec<f32> = (0..44100)
            .flat_map(|n| {
                let s = (n as f32 * std::f32::consts::TAU * 60. / 44100.).sin();
                [s, -s]
            })
            .collect();
        let packet = Packet {
            data: data.clone(),
            n: 0,
            rate: nz(44100),
            channels: ch(2),
            change: None,
            channel_change: None,
        };
        let actual: Vec<_> = convert(packet, ch(2), nz(48000), flag()).collect();
        let expected: Vec<_> = rodio::conversions::SampleRateConverter::new(
            data.into_iter(),
            nz(44100),
            nz(48000),
            ch(2),
        )
        .collect();
        assert_eq!(actual.len(), 96000);
        assert_eq!(actual, expected);
    }
    #[test]
    fn passthrough_and_empty_short_sources_are_frame_aligned() {
        for data in [vec![], vec![0.25, -0.5], vec![0.25, -0.5, 0.75, -0.9]] {
            let f = flag();
            let actual: Vec<_> = convert(
                SamplesBuffer::new(ch(2), nz(48000), data.clone()),
                ch(2),
                nz(48000),
                f.clone(),
            )
            .collect();
            assert_eq!(actual, data);
            assert!(!f.load(Ordering::Acquire));
        }
    }
    #[test]
    fn format_change_stops_without_panic_or_partial_frame() {
        let f = flag();
        let input = Packet {
            data: vec![0.2; 100],
            n: 0,
            rate: nz(44100),
            channels: ch(2),
            change: Some(8),
            channel_change: None,
        };
        let out: Vec<_> = convert(input, ch(2), nz(48000), f.clone()).collect();
        assert!(f.load(Ordering::Acquire));
        assert_eq!(out.len() % 2, 0);
        assert!(out.len() < 100);
    }
    #[test]
    fn channel_change_discards_the_whole_affected_frame() {
        let f = flag();
        let data = vec![0.25, -0.5, 0.75, -0.9, 0.1, -0.2, 0.8, -0.7];
        let input = Packet {
            data: data.clone(),
            n: 0,
            rate: nz(48000),
            channels: ch(2),
            change: None,
            // The decoder changes metadata while loading the first sample of
            // the fourth frame. Neither channel of that frame may escape.
            channel_change: Some(7),
        };
        let mut source = convert(input, ch(2), nz(48000), f.clone());
        assert_eq!(source.by_ref().collect::<Vec<_>>(), data[..6]);
        assert!(f.load(Ordering::Acquire));
        assert_eq!(source.next(), None);
        assert_eq!(source.next(), None);
    }
    #[test]
    fn truncated_input_frame_is_rejected_before_conversion() {
        for input_rate in [44100, 48000] {
            let f = flag();
            let mut source = convert(
                SamplesBuffer::new(ch(2), nz(input_rate), vec![0.25, -0.5, 0.9]),
                ch(2),
                nz(48000),
                f.clone(),
            );
            let out: Vec<_> = source.by_ref().collect();
            assert!(f.load(Ordering::Acquire));
            assert_eq!(out.len() % 2, 0);
            assert!(out.as_chunks::<2>().0.iter().all(|frame| *frame == [0.25, -0.5]));
            assert_eq!(source.next(), None);
        }
    }
    #[test]
    fn new_track_and_seek_replacement_have_no_previous_samples() {
        let mut old = convert(
            SamplesBuffer::new(ch(2), nz(44100), vec![1.0; 4410]),
            ch(2),
            nz(48000),
            flag(),
        );
        for _ in 0..31 {
            old.next();
        }
        drop(old);
        let fresh: Vec<_> = convert(
            SamplesBuffer::new(ch(2), nz(44100), vec![0.0; 4410]),
            ch(2),
            nz(48000),
            flag(),
        )
        .collect();
        assert!(fresh.iter().all(|&s| s == 0.0));
    }
    #[test]
    fn player_queue_accepts_alternating_track_formats_and_eof() {
        let (mixer, mut output) = rodio::mixer::mixer(ch(2), nz(48000));
        let player = connect_player(&mixer, ch(2), nz(48000));
        player.pause();
        player.append(convert(
            SamplesBuffer::new(ch(1), nz(44100), vec![0.25; 4410]),
            ch(2),
            nz(48000),
            flag(),
        ));
        player.append(convert(
            SamplesBuffer::new(ch(2), nz(48000), vec![-0.5; 9600]),
            ch(2),
            nz(48000),
            flag(),
        ));
        // Paused sources produce silence without consuming either finite track.
        assert!((0..1000).all(|_| output.next() == Some(0.0)));
        player.play();
        let out: Vec<_> = (0..24000).filter_map(|_| output.next()).collect();
        assert!(out.contains(&0.25));
        assert!(out.contains(&-0.5));
        assert!(player.empty());
    }
    #[test]
    fn player_keepalive_accepts_a_different_format_after_eof() {
        let (mixer, mut output) = rodio::mixer::mixer(ch(2), nz(48000));
        let player = connect_player(&mixer, ch(2), nz(48000));
        // Opus-like stereo at 48 kHz, followed after idle silence by AAC-like
        // mono at 44.1 kHz. The keepalive queue must retain its span boundaries.
        for (channels, rate, data, expected) in [
            (ch(2), nz(48000), vec![0.25; 9600], 0.25),
            (ch(1), nz(44100), vec![-0.5; 4410], -0.5),
        ] {
            let f = flag();
            player.append(convert(SamplesBuffer::new(channels, rate, data), ch(2), nz(48000), f.clone()));
            let out: Vec<_> = (0..16000).map(|_| output.next().unwrap()).collect();
            // Each 100 ms song occupies exactly 4800 stereo output frames,
            // regardless of the input format or the preceding idle silence.
            assert_eq!(out.iter().filter(|&&s| s == expected).count(), 9600, "input {channels} ch / {rate} Hz");
            assert!(player.empty());
            assert!(!f.load(Ordering::Acquire));
            assert!((0..1000).all(|_| output.next() == Some(0.0)));
        }
    }
    #[test]
    fn primed_player_is_empty_and_can_clear_without_output_consumption() {
        let (mixer, _output) = rodio::mixer::mixer(ch(6), nz(44100));
        let player = connect_player(&mixer, ch(6), nz(44100));
        assert!(player.empty());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            player.clear();
            let _ = tx.send(player.empty());
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(true));
        worker.join().unwrap();
    }
    #[test]
    fn primed_queue_preserves_samples_and_channel_order_at_start_and_after_idle() {
        for channels in [ch(1), ch(2), ch(6)] {
            for rate in [nz(44100), nz(48000), nz(96000)] {
                for idle in [false, true] {
                    let (mixer, mut output) = rodio::mixer::mixer(channels, rate);
                    let player = connect_player(&mixer, channels, rate);
                    assert!(player.empty());
                    // The engine clears a newly constructed player before Start.
                    player.clear();
                    player.play();
                    let width = channels.get() as usize;
                    if idle {
                        assert!((0..width * 1000).all(|_| output.next() == Some(0.0)));
                    }
                    // Distinct nonzero channel values expose swaps and duplicated
                    // samples that a constant mono/stereo tone would conceal.
                    let frame: Vec<_> = (1..=channels.get()).map(|n| n as f32 * 0.125).collect();
                    let expected: Vec<_> = frame.iter().copied().cycle().take(width * 1000).collect();
                    let f = flag();
                    player.append(convert(SamplesBuffer::new(channels, rate, expected.clone()), channels, rate, f.clone()));
                    let out: Vec<_> = (0..expected.len() + width * 2000).map(|_| output.next().unwrap()).collect();
                    let first = out.iter().position(|&s| s != 0.0).unwrap();
                    assert_eq!(first % width, 0, "{channels} ch / {rate} Hz, idle {idle}");
                    assert_eq!(&out[first..first + expected.len()], expected, "{channels} ch / {rate} Hz, idle {idle}");
                    assert!(out[first + expected.len()..].iter().all(|&s| s == 0.0));
                    assert!(player.empty());
                    assert!(!f.load(Ordering::Acquire));
                }
            }
        }
    }
}
