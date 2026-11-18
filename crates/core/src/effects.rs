use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Amp {
    #[default]
    Off,
    Clean,
    Crunch,
    Bass,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effects {
    pub amp: Amp,
    pub pitch: f32,
    pub formant: f32,
    pub harmony: f32,
    pub harmony_mix: f32,
}

impl Effects {
    pub fn sanitized(self) -> Self {
        fn bounded(value: f32, min: f32, max: f32) -> f32 {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                0.0
            }
        }
        Self {
            pitch: bounded(self.pitch, -12.0, 12.0),
            formant: bounded(self.formant, -1.0, 1.0),
            harmony: bounded(self.harmony, -12.0, 12.0),
            harmony_mix: bounded(self.harmony_mix, 0.0, 0.65),
            ..self
        }
    }
}

/// Implementations must be prepared before entering the render thread.
/// Processing must avoid allocations, locks and I/O.
pub trait AudioEffect: Send {
    fn process(&mut self, sample: f32) -> f32;
}

pub struct Rack {
    settings: Effects,
    pitch: Pitch,
    harmony: Pitch,
    cab: f32,
    body: f32,
    rate: f32,
    pitch_ratio: f32,
    harmony_ratio: f32,
    cab_alpha: f32,
    body_alpha: f32,
}

impl Rack {
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(8_000) as f32;
        Self {
            settings: Effects::default(),
            pitch: Pitch::new(rate),
            harmony: Pitch::new(rate),
            cab: 0.0,
            body: 0.0,
            rate,
            pitch_ratio: 1.0,
            harmony_ratio: 1.0,
            cab_alpha: 0.0,
            body_alpha: 0.0,
        }
    }

    pub fn set(&mut self, settings: Effects) {
        if self.settings == settings {
            return;
        }
        self.settings = settings.sanitized();
        let p = self.settings;
        self.pitch_ratio = 2.0_f32.powf(p.pitch / 12.0);
        self.harmony_ratio = 2.0_f32.powf((p.pitch + p.harmony) / 12.0);
        let cab_cutoff = match p.amp {
            Amp::Off => 6000.0,
            Amp::Clean => 5300.0,
            Amp::Crunch => 3600.0,
            Amp::Bass => 2400.0,
        };
        self.cab_alpha = 1.0 - (-std::f32::consts::TAU * cab_cutoff / self.rate).exp();
        let body_cutoff = 900.0 * 2.0_f32.powf(p.formant);
        self.body_alpha = 1.0 - (-std::f32::consts::TAU * body_cutoff / self.rate).exp();
    }
}

impl AudioEffect for Rack {
    fn process(&mut self, sample: f32) -> f32 {
        let p = self.settings;
        let mut signal = self.pitch.tick(sample, self.pitch_ratio);
        let harmony = self.harmony.tick(sample, self.harmony_ratio);
        signal = signal * (1.0 - p.harmony_mix) + harmony * p.harmony_mix;
        if p.formant != 0.0 {
            self.body += self.body_alpha * (signal - self.body);
            signal += p.formant * (signal - self.body * 1.7) * 0.55;
        }
        let drive = match p.amp {
            Amp::Off => return signal,
            Amp::Clean => 1.5,
            Amp::Crunch => 6.0,
            Amp::Bass => 2.2,
        };
        self.cab += self.cab_alpha * ((signal * drive).tanh() / drive.sqrt() - self.cab);
        self.cab
    }
}

struct Pitch {
    buffer: Vec<f32>,
    write: usize,
    phase: f32,
    span: f32,
}

impl Pitch {
    fn new(rate: f32) -> Self {
        let span = (rate * 0.035).ceil();
        Self {
            buffer: vec![0.0; span as usize + 4],
            write: 0,
            phase: 0.0,
            span,
        }
    }

    fn tick(&mut self, sample: f32, ratio: f32) -> f32 {
        self.buffer[self.write] = sample;
        let output = if ratio == 1.0 {
            sample
        } else {
            self.phase = (self.phase + (1.0 - ratio) / self.span).rem_euclid(1.0);
            let second = (self.phase + 0.5).fract();
            let weight = 1.0 - (2.0 * self.phase - 1.0).abs();
            let weight = weight * weight * (3.0 - 2.0 * weight);
            self.read(self.phase) * weight + self.read(second) * (1.0 - weight)
        };
        self.write = (self.write + 1) % self.buffer.len();
        output
    }

    fn read(&self, phase: f32) -> f32 {
        let len = self.buffer.len();
        let delay = 1.0 + phase * self.span;
        let index = (self.write + len - delay as usize) % len;
        let next = (index + len - 1) % len;
        let weight = delay.fract();
        self.buffer[index] * (1.0 - weight) + self.buffer[next] * weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_rack_shifts_an_octave_without_render_allocations() {
        let mut rack = Rack::new(48_000);
        rack.set(Effects {
            pitch: 12.0,
            ..Effects::default()
        });
        let mut rising_crossings = 0;
        let mut previous = 0.0;
        let allocations = crate::allocation_probe::count(|| {
            for frame in 0..52_800 {
                let phase = std::f32::consts::TAU * 220.0 * frame as f32 / 48_000.0;
                let sample = rack.process(phase.sin() * 0.25);
                assert!(sample.is_finite());
                if frame >= 4_800 && sample >= 0.0 && previous < 0.0 {
                    rising_crossings += 1;
                }
                previous = sample;
            }
        });
        assert_eq!(allocations, 0);
        assert!(
            (415..465).contains(&rising_crossings),
            "{rising_crossings} crossings"
        );
    }
}
