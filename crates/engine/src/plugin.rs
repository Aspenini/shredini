use anyhow::{Context, Result, ensure};
use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_host::{
    prelude::*,
    process::{
        PluginAudioProcessor,
        audio_buffers::{AudioPortBuffer, AudioPortBufferType, AudioPorts, InputChannel},
    },
};
use std::{
    ffi::{CStr, CString},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

pub(crate) const BLOCK: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectPlugin {
    pub name: String,
    path: PathBuf,
    id: CString,
}

impl EffectPlugin {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn scan(path: &Path) -> Result<Vec<EffectPlugin>> {
    ensure!(
        path.extension()
            .is_some_and(|value| value.eq_ignore_ascii_case("clap")),
        "Choose a .clap plugin"
    );
    let path = path.canonicalize()?;
    // SAFETY: only a locally selected CLAP binary enters this route. The loaded
    // binary must implement the CLAP ABI, and Clack owns its entry lifetime.
    let entry = unsafe { PluginEntry::load(&path) }.context("Open CLAP plugin")?;
    let factory = entry
        .get_plugin_factory()
        .context("Missing CLAP plugin factory")?;
    let plugins: Vec<_> = factory
        .plugin_descriptors()
        .filter_map(|descriptor| {
            let id = descriptor.id()?.to_owned();
            let name = descriptor
                .name()
                .unwrap_or(id.as_c_str())
                .to_string_lossy()
                .into_owned();
            Some(EffectPlugin {
                id,
                name,
                path: path.clone(),
            })
        })
        .collect();
    ensure!(!plugins.is_empty(), "No plugins in this CLAP bundle");
    Ok(plugins)
}

struct Host;

#[derive(Default)]
struct Signals {
    callback: AtomicBool,
    restart: AtomicBool,
    failed: AtomicBool,
}

struct SharedSignals(Arc<Signals>);

impl<'a> SharedHandler<'a> for SharedSignals {
    fn request_restart(&self) {
        self.0.restart.store(true, Relaxed);
    }
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.0.callback.store(true, Relaxed);
    }
}

impl HostHandlers for Host {
    type Shared<'a> = SharedSignals;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

pub(crate) struct Control {
    instance: PluginInstance<Host>,
    signals: Arc<Signals>,
}

impl Control {
    pub(crate) fn prepare(selection: &EffectPlugin, rate: u32) -> Result<(Self, Insert)> {
        // SAFETY: selection comes from scan's validated CLAP entry and descriptor.
        let entry = unsafe { PluginEntry::load(&selection.path) }.context("Reopen CLAP plugin")?;
        Self::prepare_entry(&entry, &selection.id, rate)
            .with_context(|| format!("Prepare {}", selection.name))
    }

    fn prepare_entry(entry: &PluginEntry, id: &CStr, rate: u32) -> Result<(Self, Insert)> {
        let info = HostInfo::new("Shredini", "Shredini", "", env!("CARGO_PKG_VERSION"))?;
        let signals = Arc::new(Signals::default());
        let mut instance = PluginInstance::<Host>::new(
            |_| SharedSignals(signals.clone()),
            |_| (),
            entry,
            id,
            &info,
        )
        .context("Instantiate CLAP plugin")?;
        let plugin = instance.plugin_handle();
        let ports = plugin
            .get_extension::<PluginAudioPorts>()
            .context("The plugin has no audio ports")?;
        let mut meta = AudioPortInfoBuffer::new();
        let mut channels = |input| -> Result<usize> {
            ensure!(
                ports.count(&plugin, input) == 1,
                "Choose an audio effect with one input bus and one output bus"
            );
            let port = ports
                .get(&plugin, 0, input, &mut meta)
                .context("Read plugin audio port")?;
            ensure!(
                (1..=2).contains(&port.channel_count),
                "Only mono and stereo CLAP effects are supported"
            );
            Ok(port.channel_count as usize)
        };
        let input_channels = channels(true)?;
        let output_channels = channels(false)?;
        let processor = instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: f64::from(rate),
                    min_frames_count: BLOCK as u32,
                    max_frames_count: BLOCK as u32,
                },
            )
            .context("Activate CLAP plugin")?;
        let insert = Insert {
            processor: processor.into(),
            signals: signals.clone(),
            input: [[0.0; BLOCK]; 2],
            output: [[0.0; BLOCK]; 2],
            next: [[0.0; BLOCK]; 2],
            dry: [[0.0; BLOCK]; 2],
            in_ports: AudioPorts::with_capacity(input_channels, 1),
            out_ports: AudioPorts::with_capacity(output_channels, 1),
            input_channels,
            output_channels,
            position: 0,
            time: 0,
            failed: false,
        };
        Ok((Self { instance, signals }, insert))
    }

    pub(crate) fn poll(&mut self) -> Option<&'static str> {
        if self.signals.callback.swap(false, Relaxed) {
            self.instance.call_on_main_thread_callback();
        }
        if self.signals.restart.swap(false, Relaxed) {
            Some("The CLAP effect requested a restart. Stop and start audio to reload it.")
        } else if self.signals.failed.swap(false, Relaxed) {
            Some("The CLAP effect stopped processing. Its input is passing through.")
        } else {
            None
        }
    }
}

pub(crate) struct Insert {
    processor: PluginAudioProcessor<Host>,
    signals: Arc<Signals>,
    input: [[f32; BLOCK]; 2],
    output: [[f32; BLOCK]; 2],
    next: [[f32; BLOCK]; 2],
    dry: [[f32; BLOCK]; 2],
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    input_channels: usize,
    output_channels: usize,
    position: usize,
    time: u64,
    failed: bool,
}

impl Insert {
    pub(crate) fn tick(&mut self, frame: [f32; 2], bypass: f32) -> [f32; 2] {
        let index = self.position;
        let result = std::array::from_fn(|channel| {
            let dry = self.dry[channel][index];
            let wet = self.output[channel][index];
            self.next[channel][index] = frame[channel];
            wet + (dry - wet) * bypass
        });
        self.position += 1;
        if self.position == BLOCK {
            self.render_block();
        }
        result
    }

    fn render_block(&mut self) {
        self.position = 0;
        std::mem::swap(&mut self.dry, &mut self.next);
        if self.failed {
            self.output = self.dry;
            return;
        }
        self.input = self.dry;
        if self.input_channels == 1 {
            for i in 0..BLOCK {
                self.input[0][i] = (self.dry[0][i] + self.dry[1][i]) * 0.5;
            }
        }
        self.output.fill([0.0; BLOCK]);
        let audio_in = self.in_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                self.input[..self.input_channels]
                    .iter_mut()
                    .map(InputChannel::variable),
            ),
        }]);
        let mut audio_out = self.out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                self.output[..self.output_channels]
                    .iter_mut()
                    .map(|buffer| buffer.as_mut_slice()),
            ),
        }]);
        let result = self
            .processor
            .ensure_processing_started()
            .and_then(|processor| {
                processor.process(
                    &audio_in,
                    &mut audio_out,
                    &InputEvents::empty(),
                    &mut OutputEvents::void(),
                    Some(self.time),
                    None,
                )
            });
        self.time += BLOCK as u64;
        if result.is_err() {
            self.output = self.dry;
            self.failed = true;
            self.signals.failed.store(true, Relaxed);
        } else if self.output_channels == 1 {
            self.output[1] = self.output[0];
        }
        for (channel, samples) in self.output.iter_mut().enumerate() {
            for (index, sample) in samples.iter_mut().enumerate() {
                if !sample.is_finite() {
                    *sample = self.dry[channel][index];
                }
            }
        }
    }
}

impl Drop for Insert {
    fn drop(&mut self) {
        self.processor.ensure_processing_stopped();
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, Control, PluginEntry};
    use clack_extensions::audio_ports::{
        AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
        PluginAudioPortsImpl,
    };
    use clack_plugin::prelude::*;

    struct TestEffect;
    struct StereoPorts;
    struct Processor;

    impl<'a> PluginMainThread<'a, ()> for StereoPorts {}

    impl PluginAudioPortsImpl for StereoPorts {
        fn count(&self, _: bool) -> u32 {
            1
        }

        fn get(&self, index: u32, input: bool, writer: &mut AudioPortInfoWriter) {
            if index == 0 {
                writer.set(&AudioPortInfo {
                    id: ClapId::new(u32::from(input)),
                    name: if input {
                        b"Stereo input"
                    } else {
                        b"Stereo output"
                    },
                    channel_count: 2,
                    flags: AudioPortFlags::IS_MAIN,
                    port_type: Some(AudioPortType::STEREO),
                    in_place_pair: None,
                });
            }
        }
    }

    impl Plugin for TestEffect {
        type AudioProcessor<'a> = Processor;
        type Shared<'a> = ();
        type MainThread<'a> = StereoPorts;

        fn declare_extensions(builder: &mut PluginExtensions<Self>, _: Option<&()>) {
            builder.register::<PluginAudioPorts>();
        }
    }

    impl DefaultPluginFactory for TestEffect {
        fn get_descriptor() -> PluginDescriptor {
            PluginDescriptor::new("shredini.test.gain", "Test gain")
        }
        fn new_shared(_: HostSharedHandle<'_>) -> Result<(), PluginError> {
            Ok(())
        }
        fn new_main_thread<'a>(
            _: HostMainThreadHandle<'a>,
            _: &'a (),
        ) -> Result<StereoPorts, PluginError> {
            Ok(StereoPorts)
        }
    }

    impl<'a> PluginAudioProcessor<'a, (), StereoPorts> for Processor {
        fn activate(
            _: HostAudioProcessorHandle<'a>,
            _: &StereoPorts,
            _: &'a (),
            _: PluginAudioConfiguration,
        ) -> Result<Self, PluginError> {
            Ok(Self)
        }

        fn process(
            &mut self,
            _: Process,
            mut audio: Audio,
            _: Events,
        ) -> Result<ProcessStatus, PluginError> {
            let mut port = audio.port_pair(0).unwrap();
            let mut pair = port.channels().unwrap();
            for channel in pair.as_f32_mut().unwrap().iter_mut() {
                if let ChannelPair::InputOutput(input, output) = channel {
                    for (src, dst) in input.iter().zip(output) {
                        *dst = *src * 0.5;
                    }
                }
            }
            Ok(ProcessStatus::Continue)
        }
    }

    #[test]
    fn clap_insert_processes_stereo_and_bypasses_with_matching_latency() -> anyhow::Result<()> {
        let entry =
            PluginEntry::load_from_clack::<SinglePluginEntry<TestEffect>>(c"shredini-test.clap")?;
        let (_control, mut insert) = Control::prepare_entry(&entry, c"shredini.test.gain", 48_000)?;
        let count = crate::allocation_probe::count(|| {
            for _ in 0..BLOCK {
                assert_eq!(insert.tick([0.2, 0.6], 0.0), [0.0; 2]);
            }
            for _ in 0..BLOCK {
                assert_eq!(insert.tick([0.4, 0.8], 0.0), [0.1, 0.3]);
            }
            for _ in 0..BLOCK {
                assert_eq!(insert.tick([0.0; 2], 1.0), [0.4, 0.8]);
            }
        });
        assert_eq!(count, 0);
        Ok(())
    }
}
