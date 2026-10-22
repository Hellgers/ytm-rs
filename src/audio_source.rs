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

pub fn convert<S: Source>(
    inner: S,
    channels: ChannelCount,
    rate: SampleRate,
    failed: Arc<AtomicBool>,
) -> impl Source + Send
where
    S: Send,
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
            self.channels
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
        };
        let out: Vec<_> = convert(input, ch(2), nz(48000), f.clone()).collect();
        assert!(f.load(Ordering::Acquire));
        assert_eq!(out.len() % 2, 0);
        assert!(out.len() < 100);
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
        let player = rodio::Player::connect_new(&mixer);
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
        assert!(out.iter().any(|&s| s == 0.25));
        assert!(out.iter().any(|&s| s == -0.5));
        assert!(player.empty());
    }
}
