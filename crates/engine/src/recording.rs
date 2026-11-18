use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};
use shredini_core::{
    midi::Message,
    model::{InstrumentSource, SynthSound, ToneParameters},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
    },
    thread,
};

type Frame = [f32; 2];

pub struct Take {
    /// Channel 0 is dry capture at the playback clock. Channel 1 is the dry instrument.
    pub frames: Vec<Frame>,
    pub rate: u32,
    pub source: InstrumentSource,
    pub tone: ToneParameters,
    pub sound: SynthSound,
    pub midi: Vec<(u64, Message)>,
    pub dropped: u64,
    pub reached_limit: bool,
}

#[derive(Default)]
struct Gate {
    enabled: AtomicBool,
    revision: AtomicU64,
    in_block: AtomicBool,
    dropped: AtomicU64,
}

pub(crate) struct Tap {
    gate: Arc<Gate>,
    queue: HeapProd<Frame>,
    midi: HeapProd<(u64, Message)>,
    frame: u64,
    revision: u64,
    writing: bool,
}

pub(crate) struct Recorder {
    gate: Arc<Gate>,
    queue: HeapCons<Frame>,
    midi: HeapCons<(u64, Message)>,
    take: Option<Take>,
}

pub(crate) fn pair() -> (Tap, Recorder) {
    let (producer, consumer) = HeapRb::new(65_536).split();
    let (midi_tx, midi_rx) = HeapRb::new(4096).split();
    let gate = Arc::new(Gate::default());
    (
        Tap {
            gate: gate.clone(),
            queue: producer,
            midi: midi_tx,
            frame: 0,
            revision: 0,
            writing: false,
        },
        Recorder {
            gate,
            queue: consumer,
            midi: midi_rx,
            take: None,
        },
    )
}

impl Tap {
    pub(crate) fn begin(&mut self) {
        self.gate.in_block.store(true, SeqCst);
        self.writing = self.gate.enabled.load(SeqCst);
        let revision = self.gate.revision.load(SeqCst);
        if self.writing && revision != self.revision {
            self.revision = revision;
            self.frame = 0;
        }
    }

    pub(crate) fn push(&mut self, frame: Frame) {
        if self.writing {
            if self.queue.try_push(frame).is_err() {
                self.gate.dropped.fetch_add(1, SeqCst);
            } else {
                self.frame += 1;
            }
        }
    }

    pub(crate) fn event(&mut self, message: Message) {
        if self.writing && self.midi.try_push((self.frame, message)).is_err() {
            self.gate.dropped.fetch_add(1, SeqCst);
        }
    }

    pub(crate) fn end(&mut self) {
        self.gate.in_block.store(false, SeqCst);
    }
}

impl Recorder {
    pub(crate) fn start(
        &mut self,
        rate: u32,
        source: InstrumentSource,
        tone: ToneParameters,
        sound: SynthSound,
    ) -> bool {
        if self.take.is_some() {
            return false;
        }
        self.gate.dropped.store(0, SeqCst);
        self.take = Some(Take {
            frames: Vec::new(),
            rate,
            source,
            tone,
            sound,
            midi: Vec::new(),
            dropped: 0,
            reached_limit: false,
        });
        self.gate.revision.fetch_add(1, SeqCst);
        self.gate.enabled.store(true, SeqCst);
        true
    }

    pub(crate) fn drain(&mut self) -> bool {
        let Some(take) = &mut self.take else {
            return false;
        };
        // Keep the entire take bounded regardless of the device's sample rate.
        let limit = (take.rate as usize * 600).min(28_800_000);
        let count = self.queue.occupied_len().min(limit - take.frames.len());
        take.frames.extend(self.queue.pop_iter().take(count));
        let available = self.midi.occupied_len();
        let events = available.min(200_000_usize.saturating_sub(take.midi.len()));
        take.midi.extend(self.midi.pop_iter().take(events));
        self.midi.skip(available - events);
        take.reached_limit = take.frames.len() == limit || take.midi.len() == 200_000;
        take.reached_limit
    }

    pub(crate) fn finish(&mut self) -> Option<Take> {
        self.take.as_ref()?;
        self.gate.enabled.store(false, SeqCst);
        // A callback that loaded 'enabled' before the store may still be writing.
        while self.gate.in_block.load(SeqCst) {
            thread::yield_now();
        }
        self.drain();
        self.queue.skip(self.queue.occupied_len());
        let mut take = self.take.take()?;
        take.dropped = self.gate.dropped.load(SeqCst);
        Some(take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taps_only_armed_blocks_and_keeps_midi_aligned_over_restarts() {
        let (mut tap, mut recorder) = pair();
        tap.begin();
        tap.push([1.0, 1.0]);
        tap.end();

        for _ in 0..2 {
            assert!(recorder.start(
                48_000,
                InstrumentSource::GuitarAndMidi,
                ToneParameters::default(),
                SynthSound::GrandPiano
            ));
            let allocations = crate::allocation_probe::count(|| {
                tap.begin();
                tap.event(Message::On(0, 60, 95));
                tap.push([0.1, 0.2]);
                tap.push([0.3, 0.4]);
                tap.end();
                tap.begin();
                tap.event(Message::Off(0, 60));
                tap.push([0.5, 0.6]);
                tap.end();
            });
            let take = recorder.finish().expect("recorded");
            assert_eq!(allocations, 0);
            assert_eq!(take.frames, [[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]]);
            assert_eq!(
                take.midi,
                [(0, Message::On(0, 60, 95)), (2, Message::Off(0, 60))]
            );
            assert_eq!(take.dropped, 0);
        }
    }
}
