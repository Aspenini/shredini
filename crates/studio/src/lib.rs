//! Non-destructive project data and offline rendering, independent of the desktop.
use anyhow::{Context, Result, ensure};
use ringbuf::{HeapRb, traits::Split};
use serde::{Deserialize, Serialize};
use shredini_core::{
    dsp::{Processor, db_gain, peak_protect},
    media::{self, Clip, FontPlayer},
    midi::{self, Message},
    model::{SynthSound, ToneParameters},
};
use std::{
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const RATE: u32 = 48_000;
pub const MAX_SECONDS: f64 = 600.0;
pub const MAX_TRACKS: usize = 64;

#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    version: u32,
    pub bpm: f64,
    pub tracks: Vec<Track>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            version: 1,
            bpm: 120.0,
            tracks: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Track {
    pub name: String,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub tone: Option<ToneParameters>,
    pub gain_automation: Vec<GainPoint>,
    pub content: Content,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct GainPoint {
    pub beat: f64,
    pub gain_db: f32,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Content {
    Audio(Vec<Region>),
    Midi {
        sound: SynthSound,
        notes: Vec<Note>,
        #[serde(default)]
        controllers: Vec<Controller>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Region {
    pub path: PathBuf,
    pub start_beat: f64,
    pub trim_start: f64,
    pub trim_end: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Note {
    pub key: u8,
    pub channel: u8,
    pub velocity: u8,
    pub beat: f64,
    pub duration: f64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Controller {
    pub channel: u8,
    pub beat: f64,
    pub action: ControllerAction,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum ControllerAction {
    Change(u8, u8),
    Bend(u16),
}

pub fn midi_track(
    name: String,
    sound: SynthSound,
    events: impl IntoIterator<Item = (f64, Message)>,
    end_beat: f64,
) -> Track {
    let mut held = std::collections::BTreeMap::<(u8, u8), (f64, u8)>::new();
    let mut notes = Vec::new();
    let mut controllers = Vec::new();
    for (beat, message) in events {
        let (channel, key, velocity) = match message {
            Message::On(channel, key, velocity) => (channel, key, velocity),
            Message::Off(channel, key) => (channel, key, 0),
            Message::Control(channel, control, value) => {
                controllers.push(Controller {
                    beat,
                    channel,
                    action: ControllerAction::Change(control, value),
                });
                continue;
            }
            Message::Bend(channel, value) => {
                controllers.push(Controller {
                    beat,
                    channel,
                    action: ControllerAction::Bend(value),
                });
                continue;
            }
        };
        if let Some((start, prior_velocity)) = held.remove(&(channel, key)) {
            notes.push(Note {
                key,
                channel,
                velocity: prior_velocity,
                beat: start,
                duration: (beat - start).max(0.0001),
            });
        }
        if velocity > 0 {
            held.insert((channel, key), (beat, velocity));
        }
    }
    for ((channel, key), (beat, velocity)) in held {
        notes.push(Note {
            key,
            channel,
            velocity,
            beat,
            duration: (end_beat - beat).max(0.0001),
        });
    }
    Track::new(
        name,
        Content::Midi {
            sound,
            notes,
            controllers,
        },
    )
}

impl Track {
    pub fn new(name: String, content: Content) -> Self {
        Self {
            name,
            content,
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            tone: None,
            gain_automation: Vec::new(),
        }
    }
}

impl Project {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        ensure!(
            file.metadata()?.len() <= 16 * 1024 * 1024,
            "Project file exceeds 16 MB"
        );
        let mut project: Self = serde_json::from_reader(BufReader::new(file))?;
        ensure!(project.version == 1, "Unsupported project version");
        ensure!(
            (30.0..=240.0).contains(&project.bpm),
            "Invalid project tempo"
        );
        ensure!(
            project.tracks.len() <= MAX_TRACKS,
            "Too many project tracks"
        );
        let absolute = path.canonicalize()?;
        let parent = absolute.parent().unwrap_or(Path::new("."));
        for track in &mut project.tracks {
            if let Content::Audio(regions) = &mut track.content {
                for region in regions {
                    if region.path.is_relative() {
                        region.path = parent.join(&region.path);
                    }
                }
            }
        }
        Ok(project)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut encoded = self.clone();
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = directory.canonicalize()?;
        for track in &mut encoded.tracks {
            if let Content::Audio(regions) = &mut track.content {
                for region in regions {
                    // Store media under the project directory as relative paths when possible.
                    if let Ok(relative) = region.path.strip_prefix(&directory) {
                        region.path = relative.to_owned();
                    }
                }
            }
        }
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(temp.as_file_mut(), &encoded)?;
        ensure!(
            temp.as_file().metadata()?.len() <= 16 * 1024 * 1024,
            "Project file exceeds 16 MB"
        );
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }

    pub fn push(&mut self, track: Track) -> Result<()> {
        ensure!(
            self.tracks.len() < MAX_TRACKS,
            "Studio supports up to {MAX_TRACKS} tracks"
        );
        self.tracks.push(track);
        Ok(())
    }

    pub fn render(&self) -> Result<Arc<Clip>> {
        ensure!((30.0..=240.0).contains(&self.bpm), "Invalid project tempo");
        let frames_per_beat = f64::from(RATE) * 60.0 / self.bpm;
        let mut mix = Vec::new();
        for track in self.tracks.iter().filter(|track| !track.mute) {
            let mut automation = track.gain_automation.clone();
            automation.retain(|point| point.beat.is_finite() && point.beat >= 0.0);
            automation.sort_by(|a, b| a.beat.total_cmp(&b.beat));
            let mut writer = TrackWriter {
                mix: &mut mix,
                track,
                automation: &automation,
                frames_per_beat,
            };
            match &track.content {
                Content::Audio(regions) => {
                    let mut source = Vec::new();
                    for region in regions {
                        mix_region(region, frames_per_beat, &mut source)?;
                    }
                    if !source.is_empty() {
                        writer.write(source.into_iter(), 0, true)?;
                    }
                }
                Content::Midi {
                    sound,
                    notes,
                    controllers,
                } => writer.midi(*sound, notes, controllers)?,
            }
        }
        ensure!(
            !mix.is_empty(),
            "Add an audible audio region or MIDI note before rendering"
        );
        let mut peak = 0.0_f32;
        for pair in &mut mix {
            *pair = pair.map(peak_protect);
            peak = peak.max(pair[0].abs()).max(pair[1].abs());
        }
        Ok(Arc::new(Clip {
            frames: mix,
            rate: RATE,
            peak,
        }))
    }
}

fn frames(value: f64) -> Result<usize> {
    ensure!(
        (0.0..=MAX_SECONDS * f64::from(RATE)).contains(&value),
        "Arrangement exceeds {MAX_SECONDS} seconds"
    );
    Ok(value.round() as usize)
}

fn mix_region(region: &Region, frames_per_beat: f64, out: &mut Vec<[f32; 2]>) -> Result<()> {
    let clip =
        media::decode(&region.path).with_context(|| format!("Read {}", region.path.display()))?;
    ensure!(region.trim_start.is_finite(), "Invalid audio trim");
    let first = region.trim_start.clamp(0.0, clip.duration());
    let last = region
        .trim_end
        .unwrap_or_else(|| clip.duration())
        .clamp(first, clip.duration());
    let start = frames(region.start_beat * frames_per_beat)?;
    let length = frames((last - first) * f64::from(RATE))?;
    let end = frames((start + length) as f64)?;
    if length == 0 {
        return Ok(());
    }
    if out.len() < end {
        out.resize(end, [0.0; 2]);
    }
    for (index, dest) in out[start..end].iter_mut().enumerate() {
        let sample = clip.at((first + index as f64 / f64::from(RATE)) * f64::from(clip.rate));
        dest[0] += sample[0];
        dest[1] += sample[1];
    }
    Ok(())
}

struct TrackWriter<'a> {
    mix: &'a mut Vec<[f32; 2]>,
    track: &'a Track,
    automation: &'a [GainPoint],
    frames_per_beat: f64,
}

impl TrackWriter<'_> {
    fn write(
        &mut self,
        input: impl Iterator<Item = [f32; 2]>,
        start: usize,
        tail: bool,
    ) -> Result<()> {
        let tone = self.track.tone.map(|mut p| {
            p.output_db = 0.0;
            p.sanitize();
            p
        });
        let mut processor = tone.map(|p| {
            std::array::from_fn::<_, 2, _>(|_| {
                let mut dsp = Processor::new(RATE, p);
                dsp.use_full_instrument_range(RATE);
                dsp
            })
        });
        let bypass = if tone.is_some_and(|p| p.bypass) {
            1.0
        } else {
            0.0
        };
        let tail = if tail && tone.is_some_and(|p| !p.bypass && (p.reverb > 0.0 || p.delay > 0.0)) {
            RATE as usize
        } else {
            0
        };
        let pan = self.track.pan.clamp(-1.0, 1.0);
        let pan_gain = [1.0 - pan.max(0.0), 1.0 + pan.min(0.0)];
        let mut points = self.automation.iter().peekable();
        let mut previous: Option<&GainPoint> = None;
        for (offset, input) in input.chain(std::iter::repeat_n([0.0; 2], tail)).enumerate() {
            let index = start + offset;
            if index >= RATE as usize * MAX_SECONDS as usize {
                break;
            }
            let beat = index as f64 / self.frames_per_beat;
            while points.peek().is_some_and(|p| p.beat <= beat) {
                previous = points.next();
            }
            let automation = match (previous, points.peek()) {
                (Some(a), Some(b)) => {
                    let fraction = ((beat - a.beat) / (b.beat - a.beat)) as f32;
                    a.gain_db + (b.gain_db - a.gain_db) * fraction
                }
                (Some(a), None) => a.gain_db,
                (None, Some(first)) if first.beat > 0.0 => {
                    first.gain_db * (beat / first.beat) as f32
                }
                (None, _) => 0.0,
            };
            let gain = db_gain((self.track.gain_db + automation).clamp(-90.0, 18.0));
            let input = match &mut processor {
                Some(dsp) => std::array::from_fn(|channel| {
                    dsp[channel].tick(input[channel], bypass)[channel]
                }),
                None => input,
            };
            if self.mix.len() <= index {
                self.mix.resize(index + 1, [0.0; 2]);
            }
            for channel in 0..2 {
                self.mix[index][channel] += input[channel] * gain * pan_gain[channel];
            }
        }
        Ok(())
    }

    fn midi(
        &mut self,
        sound: SynthSound,
        notes: &[Note],
        controllers: &[Controller],
    ) -> Result<()> {
        if notes.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(notes.len() * 2 + controllers.len());
        for event in controllers {
            let frame = frames(event.beat * self.frames_per_beat)?;
            let message = match event.action {
                ControllerAction::Change(control, value) => {
                    Message::Control(event.channel, control, value)
                }
                ControllerAction::Bend(value) => Message::Bend(event.channel, value),
            };
            events.push((frame, 0, message));
        }
        for note in notes
            .iter()
            .filter(|note| note.key < 128 && note.velocity > 0 && note.duration > 0.0)
        {
            let on = frames(note.beat * self.frames_per_beat)?;
            let off = frames((note.beat + note.duration) * self.frames_per_beat)?.max(on + 1);
            events.push((on, 2, Message::On(note.channel, note.key, note.velocity)));
            events.push((off, 1, Message::Off(note.channel, note.key)));
        }
        if events.is_empty() {
            return Ok(());
        }
        events.sort_by_key(|event| (event.0, event.1));
        let first = events[0].0;
        let last = events.last().unwrap().0;
        let end = last.saturating_add(RATE as usize);
        let state = Arc::new(midi::State::default());
        state.set_sound(sound);
        let (_, queue) = HeapRb::new(1).split();
        let mut instrument = midi::Instrument::new(queue, state, RATE);
        if sound.program().is_some() {
            instrument.font = Some(FontPlayer::new(&media::default_font()?, RATE)?);
            instrument.refresh_font();
        }
        let mut events = events.into_iter().peekable();
        let signal = (first..end).map(|index| {
            while events.peek().is_some_and(|event| event.0 == index) {
                if let Some((_, _, message)) = events.next() {
                    instrument.dispatch(message);
                }
            }
            // Release a held sustain pedal at the end of the performance.
            if index == last.saturating_add(1) {
                for channel in 0..16 {
                    instrument.dispatch(Message::Control(channel, 64, 0));
                }
            }
            [instrument.tick(); 2]
        });
        self.write(signal, first, true)
    }
}

pub use shredini_core::wav::export_wav;

pub fn import_midi(path: &Path) -> Result<Vec<Track>> {
    use midly::{MidiMessage, Timing, TrackEventKind};
    ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "MIDI file exceeds 8 MB"
    );
    let data = fs::read(path)?;
    let midi = midly::Smf::parse(&data)?;
    let Timing::Metrical(ppq) = midi.header.timing else {
        anyhow::bail!("SMPTE MIDI timing is unsupported");
    };
    ensure!(ppq.as_int() > 0, "Invalid MIDI time division");
    let scale = f64::from(ppq.as_int());
    let mut tracks = Vec::new();
    for (index, track) in midi.tracks.iter().enumerate() {
        let end = track
            .iter()
            .map(|event| u64::from(event.delta.as_int()))
            .sum::<u64>() as f64
            / scale;
        let mut tick = 0;
        let events = track.iter().filter_map(|event| {
            tick += u64::from(event.delta.as_int());
            let TrackEventKind::Midi { channel, message } = event.kind else {
                return None;
            };
            let channel = channel.as_int();
            let message = match message {
                MidiMessage::NoteOn { key, vel } => {
                    Message::On(channel, key.as_int(), vel.as_int())
                }
                MidiMessage::NoteOff { key, .. } => Message::Off(channel, key.as_int()),
                MidiMessage::Controller { controller, value } => {
                    Message::Control(channel, controller.as_int(), value.as_int())
                }
                MidiMessage::PitchBend { bend } => {
                    Message::Bend(channel, (i32::from(bend.as_int()) + 8192) as u16)
                }
                _ => return None,
            };
            Some((tick as f64 / scale, message))
        });
        let track = midi_track(
            format!("MIDI {}", index + 1),
            SynthSound::GrandPiano,
            events,
            end,
        );
        if matches!(&track.content, Content::Midi { notes, .. } if !notes.is_empty()) {
            tracks.push(track);
        }
    }
    ensure!(!tracks.is_empty(), "The MIDI file contains no notes");
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shredini_core::effects::Amp;

    #[test]
    fn arrangement_keeps_source_pcm_and_round_trips_editable_effects() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.wav");
        let canonical = directory.path().canonicalize()?.join("source.wav");
        export_wav(&path, RATE, 2, (0..RATE).flat_map(|_| [0.25, 0.0]))?;
        let original = fs::read(&path)?;
        let mut project = Project::default();
        let mut track = Track::new(
            "Left input".into(),
            Content::Audio(vec![Region {
                path: canonical.clone(),
                start_beat: 1.0,
                trim_start: 0.1,
                trim_end: Some(0.2),
                duration: Some(1.0),
            }]),
        );
        track.pan = -0.5;
        track.gain_automation = vec![GainPoint {
            beat: 1.0,
            gain_db: -6.0,
        }];
        project.push(track)?;
        let mix = project.render()?;
        assert_eq!(mix.frames.len(), 28_800);
        assert_eq!(mix.frames[23_999], [0.0; 2]);
        assert!(mix.frames[25_000][0] > 0.12);
        assert_eq!(mix.frames[25_000][1], 0.0);
        let mut tone = ToneParameters::default();
        tone.effects.amp = Amp::Crunch;
        project.tracks[0].tone = Some(tone);
        let song = directory.path().join("song.shredini");
        project.save(&song)?;
        project.save(&song)?;
        let reloaded = Project::load(&song)?;
        assert_eq!(reloaded.tracks[0].tone, Some(tone));
        let Content::Audio(regions) = &reloaded.tracks[0].content else {
            unreachable!()
        };
        assert_eq!(regions[0].path, canonical);
        assert_eq!(fs::read(&path)?, original);
        assert_ne!(reloaded.render()?.frames[25_000], mix.frames[25_000]);
        Ok(())
    }

    #[test]
    fn midi_import_and_edited_note_render_at_the_project_tempo() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("part.mid");
        // Format 0, PPQ 96, middle C at beat one for one beat.
        fs::write(&path, b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x0c\x60\x90\x3c\x68\x60\x80\x3c\0\0\xff\x2f\0")?;
        let mut tracks = import_midi(&path)?;
        let Content::Midi { notes, .. } = &mut tracks[0].content else {
            unreachable!()
        };
        assert_eq!((notes[0].beat, notes[0].duration), (1.0, 1.0));
        notes[0].key = 67;
        let project = Project {
            tracks,
            ..Project::default()
        };
        let result = project.render()?;
        assert!(
            result.frames[..24_000]
                .iter()
                .all(|frame| *frame == [0.0; 2])
        );
        assert!(
            result.frames[24_000..48_000]
                .iter()
                .any(|frame| frame[0].abs() > 0.01)
        );
        Ok(())
    }
}
