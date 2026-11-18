use crate::{
    media, midi,
    model::{self, Backend, InstrumentSource, Preset, SharedParameters, SynthSound},
    storage::{self, Session},
};
use gpui::{
    App, Application, Bounds, Context, Div, IntoElement, SharedString, Timer, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use shredini_core::effects::{Amp, Effects};
use shredini_engine::{Command, DeviceInfo, EffectPlugin, Engine, Event, RunningInfo, Take};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::Ordering::Relaxed,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

#[cfg(feature = "studio")]
mod studio;

const BG: u32 = 0x101116;
const SIDE: u32 = 0x15161d;
const CARD: u32 = 0x1a1c25;
const LINE: u32 = 0x2d303d;
const TEXT: u32 = 0xf0eff7;
const MUTED: u32 = 0xa3a6b9;
const ACCENT: u32 = 0xc4b5fd;
const GREEN: u32 = 0x82dfb5;
const RED: u32 = 0xff9da6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Live,
    Sound,
    Vocals,
    Library,
    Setup,
    #[cfg(feature = "studio")]
    Studio,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::Sound => "Sound design",
            Self::Vocals => "Vocals",
            Self::Library => "Sound library",
            Self::Setup => "Audio & MIDI",
            #[cfg(feature = "studio")]
            Self::Studio => "Studio",
        }
    }
}

enum Job {
    Import(media::Role, PathBuf),
    ScanPlugin(PathBuf),
    SavedTake(Result<SavedTake, (anyhow::Error, Take)>),
    RetryTake(Option<PathBuf>),
    #[cfg(feature = "studio")]
    Studio(studio::Job),
}

struct SavedTake {
    paths: Vec<PathBuf>,
    #[cfg(feature = "studio")]
    take: Take,
    #[cfg(feature = "studio")]
    duration: f64,
}

struct Workspace {
    page: Page,
    session: Session,
    settings: Option<PathBuf>,
    last_change: Option<Instant>,
    engine: Engine,
    parameters: Arc<SharedParameters>,
    importer: media::Importer,
    devices: Vec<DeviceInfo>,
    ports: Vec<midi::Port>,
    running: Option<RunningInfo>,
    pending: bool,
    recording: bool,
    take_pending: bool,
    take_saving: bool,
    take_picker: bool,
    unsaved_take: Option<Take>,
    last_take: Vec<PathBuf>,
    backing_wave: Vec<f32>,
    sample_wave: Vec<f32>,
    favorites_only: bool,
    device_notice: String,
    midi_notice: String,
    plugins: Vec<EffectPlugin>,
    plugin: Option<EffectPlugin>,
    audition_next: bool,
    backing_next: bool,
    notice: String,
    meters: [f32; 2],
    jobs: Receiver<Job>,
    send_job: Sender<Job>,
    #[cfg(feature = "studio")]
    studio: studio::Editor,
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let size = size(px(1220.), px(820.));
        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size, cx))),
                window_min_size: Some(gpui::size(px(890.), px(630.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Shredini".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let workspace = cx.new(Workspace::new);
                let weak = workspace.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |view, cx| {
                        if view.recording
                            || view.take_pending
                            || view.take_saving
                            || view.unsaved_take.is_some()
                        {
                            view.notice =
                                "Finish the take and wait for it to save before closing.".into();
                            cx.notify();
                            false
                        } else {
                            if view.last_change.is_some() {
                                view.persist();
                            }
                            #[cfg(feature = "studio")]
                            view.save_recovery_on_close();
                            true
                        }
                    })
                    .unwrap_or(true)
                });
                workspace
            },
        ) {
            eprintln!("Could not open Shredini: {error}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut settings = storage::session_path();
        let (mut session, notice) = settings.as_deref().map(storage::load).unwrap_or_default();
        if notice
            .as_ref()
            .is_some_and(|n| n.contains("Saving is disabled"))
        {
            settings = None;
        }
        if std::env::args().any(|arg| arg == "--keyboard") {
            session.audio.source = InstrumentSource::MidiKeyboard;
        }
        let parameters = Arc::new(SharedParameters::new(session.parameters));
        let engine = Engine::new(parameters.clone());
        engine.media.publish(&session.media);
        let importer = media::Importer::new();
        for (role, path) in [
            (media::Role::Backing, &session.backing_path),
            (media::Role::Sample, &session.sample_path),
            (media::Role::SoundFont, &session.soundfont_path),
        ] {
            if let Some(path) = path {
                importer.import(role, path.clone());
            }
        }
        let (send_job, jobs) = mpsc::channel();
        cx.spawn(async move |weak, cx| {
            loop {
                Timer::after(Duration::from_millis(60)).await;
                if weak
                    .update(cx, |view, cx| {
                        if view.poll() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.on_app_quit(|view, _| {
            if view.last_change.is_some() {
                view.persist();
            }
            async {}
        })
        .detach();
        Self {
            page: if std::env::args().any(|arg| arg == "--setup") {
                Page::Setup
            } else {
                Page::Live
            },
            session,
            settings,
            engine,
            parameters,
            importer,
            jobs,
            send_job,
            devices: Vec::new(),
            ports: Vec::new(),
            running: None,
            pending: false,
            recording: false,
            take_pending: false,
            take_saving: false,
            take_picker: false,
            unsaved_take: None,
            last_take: Vec::new(),
            backing_wave: Vec::new(),
            sample_wave: Vec::new(),
            favorites_only: false,
            device_notice: String::new(),
            midi_notice: String::new(),
            plugins: Vec::new(),
            plugin: None,
            audition_next: false,
            backing_next: false,
            last_change: None,
            notice: notice.unwrap_or_default(),
            meters: [0.0; 2],
            #[cfg(feature = "studio")]
            studio: studio::Editor::restore(),
        }
    }

    fn changed(&mut self) {
        self.parameters.publish(self.session.parameters);
        self.engine.media.publish(&self.session.media);
        self.last_change = Some(Instant::now());
    }

    fn persist(&mut self) {
        self.last_change = None;
        if let Some(path) = &self.settings
            && let Err(error) = storage::save(path, &self.session)
        {
            self.notice = format!("Could not save settings · {error:#}");
        }
    }

    fn send(&mut self, command: Command) -> bool {
        if self.engine.send(command) {
            true
        } else {
            self.notice = "Audio controls are busy. Try again.".into();
            false
        }
    }

    fn start(&mut self, source: InstrumentSource) {
        if self.pending || self.running.is_some() {
            return;
        }
        let mut config = self.session.audio.clone();
        config.source = source;
        if config.output_id.is_empty() {
            config.output_id = self
                .devices
                .iter()
                .find(|device| {
                    device.backend == config.backend
                        && device.default_output
                        && device.output_channels >= 2
                })
                .map_or(String::new(), |device| device.id.clone());
        }
        if config.output_id.is_empty() || (source.has_audio() && config.input_id.is_empty()) {
            self.notice = "Choose your input and output in Audio & MIDI.".into();
            self.page = Page::Setup;
            self.audition_next = false;
            self.backing_next = false;
            return;
        }
        self.pending = self.send(Command::Start(config));
    }

    fn audition(&mut self) {
        if self
            .running
            .as_ref()
            .is_some_and(|info| info.source.has_midi())
        {
            self.engine.midi.test_sound();
        } else if self.running.is_some() {
            self.notice = "Stop monitoring, then switch to MIDI or audition the sound.".into();
        } else if !self.pending {
            self.audition_next = true;
            self.start(InstrumentSource::MidiKeyboard);
        }
    }

    fn poll(&mut self) -> bool {
        let mut refresh = false;
        while let Ok(event) = self.engine.events.try_recv() {
            refresh = true;
            match event {
                Event::Devices(devices, notice) => {
                    self.devices = devices;
                    self.pick_default_output();
                    self.device_notice = notice;
                }
                Event::MidiDevices(ports, notice) => {
                    self.ports = ports;
                    if self.session.audio.midi_port.is_empty()
                        && let Some(port) = self
                            .ports
                            .iter()
                            .find(|port| port.name.to_lowercase().contains("mpk mini"))
                    {
                        self.session.audio.midi_port = port.id.clone();
                        if self.session.audio.input_id.is_empty() {
                            self.session.audio.source = InstrumentSource::MidiKeyboard;
                        }
                        self.changed();
                    }
                    self.midi_notice = notice;
                }
                Event::Running(info) => {
                    self.running = Some(info);
                    self.pending = false;
                    self.notice.clear();
                    if std::mem::take(&mut self.audition_next) {
                        self.engine.midi.test_sound();
                    }
                    if std::mem::take(&mut self.backing_next) {
                        self.engine.media.playing.store(true, Relaxed);
                    }
                    #[cfg(feature = "studio")]
                    if let Some(mix) = self.studio.play_when_running.take() {
                        self.send(Command::PlayMix(mix));
                    }
                }
                Event::Stopped => {
                    self.running = None;
                    self.pending = false;
                    self.meters = [0.0; 2];
                }
                Event::Failed(error) => {
                    self.pending = false;
                    self.running = None;
                    self.take_pending = false;
                    self.audition_next = false;
                    self.backing_next = false;
                    self.meters = [0.0; 2];
                    self.notice = error;
                    #[cfg(feature = "studio")]
                    {
                        self.studio.play_when_running = None;
                    }
                }
                Event::Notice(notice) => {
                    self.notice = notice;
                    self.take_pending = false;
                }
                Event::Plugins(plugins) => {
                    let total = plugins.len();
                    self.plugins = plugins;
                    self.notice = format!("Found {total} CLAP plugin(s). Select an effect below.");
                }
                Event::PluginSelected(plugin) => {
                    self.notice = plugin.as_ref().map_or_else(
                        || "CLAP insert removed.".into(),
                        |plugin| format!("{} will load when audio starts.", plugin.name),
                    );
                    self.plugin = plugin;
                }
                Event::Recording => {
                    self.recording = true;
                    self.take_pending = false;
                }
                Event::Take(take) => {
                    self.recording = false;
                    self.take_pending = false;
                    if take.frames.is_empty() {
                        self.notice = "No audio was captured.".into();
                    } else {
                        self.save_take(take, None);
                    }
                }
            }
        }
        while let Ok(result) = self.importer.results.try_recv() {
            refresh = true;
            match result.result {
                Ok(media::Loaded::Audio(role, clip, wave))
                    if self.send(Command::InstallAudio(role, clip.clone())) =>
                {
                    let target = if role == media::Role::Backing {
                        &mut self.session.backing_path
                    } else {
                        &mut self.session.sample_path
                    };
                    *target = Some(result.path.clone());
                    if role == media::Role::Backing {
                        self.backing_wave = wave;
                    } else {
                        self.sample_wave = wave;
                    }
                    if let Some(entry) = self
                        .session
                        .library
                        .iter_mut()
                        .find(|entry| entry.path == result.path)
                    {
                        entry.role = role;
                    } else {
                        self.session.library.push(media::LibraryEntry {
                            path: result.path.clone(),
                            role,
                        });
                    }
                    self.notice = format!(
                        "Loaded {} · {:.1} s",
                        filename(&result.path),
                        clip.duration()
                    );
                    self.changed();
                }
                Ok(media::Loaded::Font(font)) if self.send(Command::InstallFont(font.clone())) => {
                    self.session.soundfont_path = Some(result.path);
                    self.notice = "SoundFont loaded.".into();
                    self.changed();
                }
                Ok(_) => {}
                Err(error) => {
                    self.notice = format!("Could not load {} · {error:#}", filename(&result.path))
                }
            }
        }
        while let Ok(job) = self.jobs.try_recv() {
            refresh = true;
            match job {
                Job::Import(role, path) => {
                    if !self.importer.import(role, path) {
                        self.notice = "The import queue is full.".into();
                    }
                }
                Job::ScanPlugin(path) => {
                    self.send(Command::ScanPlugin(path));
                }
                Job::SavedTake(result) => {
                    self.take_saving = false;
                    match result {
                        Ok(saved) => {
                            self.last_take = saved.paths.clone();
                            if !self.notice.starts_with("Recording saved with") {
                                self.notice = format!(
                                    "Dry recording saved · {}",
                                    self.last_take[0].display()
                                );
                            }
                            #[cfg(feature = "studio")]
                            self.insert_take(saved);
                        }
                        Err((error, take)) => {
                            self.notice = format!(
                                "The take is still in memory. Could not save recording · {error:#}"
                            );
                            self.unsaved_take = Some(take);
                        }
                    }
                }
                Job::RetryTake(directory) => {
                    self.take_picker = false;
                    if let Some(directory) = directory
                        && let Some(take) = self.unsaved_take.take()
                    {
                        self.save_take(take, Some(directory));
                    }
                }
                #[cfg(feature = "studio")]
                Job::Studio(job) => self.apply_studio_job(job),
            }
        }
        if self
            .last_change
            .is_some_and(|when| when.elapsed() >= Duration::from_millis(300))
        {
            self.persist();
            refresh = true;
        }
        if self.running.is_some() {
            self.meters = [
                &self.engine.diagnostics.input_peak,
                &self.engine.diagnostics.output_peak,
            ]
            .map(|peak| f32::from_bits(peak.swap(0, Relaxed)));
            refresh = true;
        }
        refresh
    }

    fn pick_default_output(&mut self) {
        let config = &mut self.session.audio;
        if !Backend::available().contains(&config.backend) {
            config.backend = Backend::standard();
        }
        if config.output_id.is_empty()
            && let Some(device) = self
                .devices
                .iter()
                .find(|d| d.backend == config.backend && d.default_output && d.output_channels >= 2)
        {
            config.output_id = device.id.clone();
            self.changed();
        }
    }

    fn save_take(&mut self, take: Take, directory: Option<PathBuf>) {
        self.take_saving = true;
        self.notice = if take.dropped > 0 || take.reached_limit {
            format!(
                "Recording saved with {} dropped samples or events{}",
                take.dropped,
                if take.reached_limit {
                    " · take reached its limit"
                } else {
                    ""
                }
            )
        } else {
            "Saving dry recording…".into()
        };
        let directory = directory
            .or_else(|| {
                storage::session_path().and_then(|path| path.parent().map(|p| p.join("Takes")))
            })
            .unwrap_or_else(|| std::env::temp_dir().join("Shredini Takes"));
        let tx = self.send_job.clone();
        std::thread::spawn(move || {
            let result = match write_take(&take, &directory) {
                Ok(paths) => {
                    #[cfg(feature = "studio")]
                    let duration = take.frames.len() as f64 / f64::from(take.rate);
                    #[cfg(feature = "studio")]
                    let take = Take {
                        frames: Vec::new(),
                        ..take
                    };
                    Ok(SavedTake {
                        paths,
                        #[cfg(feature = "studio")]
                        take,
                        #[cfg(feature = "studio")]
                        duration,
                    })
                }
                Err(error) => Err((error, take)),
            };
            let _ = tx.send(Job::SavedTake(result));
        });
    }

    fn retry_take(&mut self) {
        self.take_picker = true;
        let tx = self.send_job.clone();
        std::thread::spawn(move || {
            let directory = rfd::FileDialog::new()
                .set_title("Choose a folder for the recording")
                .pick_folder();
            let _ = tx.send(Job::RetryTake(directory));
        });
    }

    fn drop_media(&mut self, paths: &[PathBuf]) {
        #[cfg(feature = "studio")]
        if self.page == Page::Studio {
            self.import_dropped(paths.to_vec());
            return;
        }
        for path in paths {
            let extension = path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            match extension.as_str() {
                "sf2" => {
                    self.importer.import(media::Role::SoundFont, path.clone());
                }
                "wav" | "flac" | "mp3" | "ogg" => {
                    let role = if self.page == Page::Library {
                        media::Role::Sample
                    } else {
                        media::Role::Backing
                    };
                    self.importer.import(role, path.clone());
                }
                _ => {
                    self.notice = format!("Unsupported file · {}", filename(path));
                }
            }
        }
    }

    fn choose_file(&self, role: media::Role) {
        let tx = self.send_job.clone();
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new();
            let dialog = if role == media::Role::SoundFont {
                dialog.add_filter("SoundFont", &["sf2"])
            } else {
                dialog.add_filter("Audio", &["wav", "flac", "mp3", "ogg"])
            };
            if let Some(path) = dialog.pick_file() {
                let _ = tx.send(Job::Import(role, path));
            }
        });
    }

    fn set_sound(&mut self, sound: SynthSound) {
        self.session.audio.synth_sound = sound;
        self.engine.midi.set_sound(sound);
        self.changed();
    }

    fn load_preset(&mut self, preset: Preset) {
        let previous = self.session.parameters;
        self.session.parameters = model::ToneParameters {
            input_trim_db: previous.input_trim_db,
            output_db: previous.output_db,
            bypass: previous.bypass,
            ..preset.parameters
        };
        self.session.current_preset = preset.name;
        self.changed();
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Live => self.live(cx),
            Page::Sound => self.sound(cx),
            Page::Vocals => self.vocals(cx),
            Page::Library => self.library(cx),
            Page::Setup => self.setup(cx),
            #[cfg(feature = "studio")]
            Page::Studio => self.studio_page(cx),
        };
        let mut nav = vec![Page::Live, Page::Sound, Page::Vocals, Page::Library];
        #[cfg(feature = "studio")]
        nav.push(Page::Studio);
        nav.push(Page::Setup);
        let sidebar = column()
            .w(px(202.))
            .flex_shrink_0()
            .h_full()
            .bg(rgb(SIDE))
            .border_r_1()
            .border_color(rgb(LINE))
            .px_4()
            .py_5()
            .gap_2()
            .child(
                div()
                    .px_2()
                    .pb_6()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("Shredini"),
            )
            .children(nav.into_iter().map(|page| {
                let selected = page == self.page;
                div()
                    .id(page.label())
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if selected { 0x30284c } else { SIDE }))
                    .text_color(rgb(if selected { ACCENT } else { MUTED }))
                    .hover(|s| s.bg(rgb(LINE)))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.page = page;
                        cx.notify();
                    }))
                    .child(page.label())
            }));
        div()
            .flex()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(sidebar)
            .child(
                column()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        column()
                            .id(("workspace-content", self.page as usize))
                            .on_drop(cx.listener(|view, paths: &gpui::ExternalPaths, _, cx| {
                                view.drop_media(paths.paths());
                                cx.notify();
                            }))
                            .overflow_y_scroll()
                            .flex_1()
                            .min_h_0()
                            .px_7()
                            .py_6()
                            .gap_5()
                            .child(page),
                    )
                    .child(self.transport(cx)),
            )
    }
}

fn write_take(take: &Take, directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(directory)?;
    let directory = directory.canonicalize()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut paths = Vec::new();
    for (index, role, active) in [
        (0, "Input", take.source.has_audio()),
        (1, "Instrument", take.source.has_midi()),
    ] {
        if active {
            let path = directory.join(format!("Take {stamp} {role}.wav"));
            shredini_audio_core::wav::export_wav(
                &path,
                take.rate,
                1,
                take.frames.iter().map(|frame| frame[index]),
            )?;
            paths.push(path);
        }
    }
    Ok(paths)
}

fn column() -> Div {
    div().flex().flex_col()
}
fn row() -> Div {
    div().flex().items_center().gap_2()
}
fn wrap() -> Div {
    row().flex_wrap()
}
fn card() -> Div {
    column()
        .gap_3()
        .p_4()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(LINE))
}
fn muted(value: impl Into<SharedString>) -> Div {
    div().text_color(rgb(MUTED)).child(value.into())
}
fn title(value: &'static str, subtitle: &'static str) -> Div {
    column()
        .gap_1()
        .child(
            div()
                .text_3xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child(value),
        )
        .child(muted(subtitle))
}
fn heading(value: impl Into<SharedString>) -> Div {
    div()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(value.into())
}
fn filename(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[derive(Clone, Copy)]
enum Style {
    Normal,
    Active,
    Primary,
    Danger,
    Disabled,
}

fn button(
    cx: &mut Context<Workspace>,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    style: Style,
    action: impl Fn(&mut Workspace) + 'static,
) -> gpui::Stateful<Div> {
    let (background, color) = match style {
        Style::Normal => (0x252834, TEXT),
        Style::Active => (0x352c52, ACCENT),
        Style::Primary => (ACCENT, 0x1a122e),
        Style::Danger => (0x532d3a, RED),
        Style::Disabled => (0x21222b, 0x696d81),
    };
    let item = div()
        .id(id)
        .px_3()
        .py_1p5()
        .rounded_md()
        .bg(rgb(background))
        .text_color(rgb(color))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(label.into());
    if matches!(style, Style::Disabled) {
        item
    } else {
        item.cursor_pointer()
            .hover(|s| s.opacity(0.86))
            .on_click(cx.listener(move |view, _, _, cx| {
                action(view);
                cx.notify();
            }))
    }
}

fn state(selected: bool) -> Style {
    if selected {
        Style::Active
    } else {
        Style::Normal
    }
}

fn control(
    cx: &mut Context<Workspace>,
    label: &'static str,
    value: String,
    apply: impl Fn(&mut Workspace, f32) + Copy + 'static,
) -> Div {
    let step = move |view: &mut Workspace, delta| {
        apply(view, delta);
    };
    row().w_full().justify_between().child(muted(label)).child(
        row()
            .child(button(
                cx,
                (label, 0_usize),
                "−",
                Style::Normal,
                move |view| step(view, -1.0),
            ))
            .child(div().w(px(67.)).text_center().child(value))
            .child(button(
                cx,
                (label, 1_usize),
                "+",
                Style::Normal,
                move |view| step(view, 1.0),
            )),
    )
}

fn meter(label: &'static str, peak: f32) -> Div {
    let db = 20.0 * peak.max(0.001).log10();
    let ratio = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    column()
        .gap_1()
        .w(px(96.))
        .child(muted(label).text_xs())
        .child(
            div().w_full().h(px(4.)).rounded_full().bg(rgb(LINE)).child(
                div()
                    .w(px(96. * ratio))
                    .h_full()
                    .rounded_full()
                    .bg(rgb(if peak > 0.98 { RED } else { GREEN })),
            ),
        )
}

impl Workspace {
    fn transport(&self, cx: &mut Context<Self>) -> Div {
        let active = self.running.is_some();
        let enabled = self
            .running
            .as_ref()
            .is_some_and(|r| r.source != InstrumentSource::Playback);
        let (label, style) = if self.pending {
            (
                if active { "Stopping…" } else { "Starting…" },
                Style::Disabled,
            )
        } else if active {
            ("Stop audio", Style::Normal)
        } else {
            ("Start audio", Style::Primary)
        };
        #[cfg(feature = "studio")]
        let editing_project = self.studio.busy;
        #[cfg(not(feature = "studio"))]
        let editing_project = false;
        let record_style = if self.pending
            || self.take_saving
            || self.take_pending
            || self.unsaved_take.is_some()
            || (!self.recording && editing_project)
            || !(enabled || self.recording)
        {
            Style::Disabled
        } else if self.recording {
            Style::Danger
        } else {
            Style::Normal
        };
        let device_label = self.running.as_ref().map_or_else(
            || "Standby".into(),
            |info| {
                format!(
                    "{:.1} kHz · {}",
                    info.output_rate as f32 / 1000.0,
                    info.source.label()
                )
            },
        );
        column()
            .gap_2()
            .px_5()
            .py_3()
            .bg(rgb(SIDE))
            .border_t_1()
            .border_color(rgb(LINE))
            .when(!self.notice.is_empty(), |this| {
                this.child(muted(self.notice.clone()).text_xs())
            })
            .when(self.unsaved_take.is_some(), |panel| {
                panel.child(
                    row()
                        .child(button(
                            cx,
                            "retry-take",
                            "Save take to folder…",
                            if self.take_picker {
                                Style::Disabled
                            } else {
                                Style::Primary
                            },
                            Workspace::retry_take,
                        ))
                        .child(button(
                            cx,
                            "discard-take",
                            "Discard unsaved take",
                            if self.take_picker {
                                Style::Disabled
                            } else {
                                Style::Danger
                            },
                            |view| {
                                view.unsaved_take = None;
                                view.notice = "Unsaved take discarded.".into();
                            },
                        )),
                )
            })
            .child(
                wrap()
                    .gap_4()
                    .child(button(cx, "audio-transport", label, style, |view| {
                        if view.running.is_some() {
                            view.pending = view.send(Command::Stop);
                        } else {
                            view.start(view.session.audio.source);
                        }
                    }))
                    .child(button(
                        cx,
                        "record",
                        if self.recording {
                            "Finish take"
                        } else {
                            "● Record"
                        },
                        record_style,
                        |view| {
                            #[cfg(feature = "studio")]
                            if !view.recording {
                                view.capture_anchor();
                            }
                            view.take_pending = view.send(if view.recording {
                                Command::FinishTake
                            } else {
                                Command::Record
                            });
                        },
                    ))
                    .child(
                        div()
                            .text_color(rgb(if active { GREEN } else { MUTED }))
                            .child(device_label),
                    )
                    .child(div().flex_1())
                    .child(meter("INPUT", self.meters[0]))
                    .child(meter("OUTPUT", self.meters[1]))
                    .child(button(cx, "master-down", "−", Style::Normal, |view| {
                        view.session.parameters.output_db =
                            (view.session.parameters.output_db - 2.0).max(-60.0);
                        view.changed();
                    }))
                    .child(muted(format!(
                        "{:.0} dB",
                        self.session.parameters.output_db
                    )))
                    .child(button(cx, "master-up", "+", Style::Normal, |view| {
                        view.session.parameters.output_db =
                            (view.session.parameters.output_db + 2.0).min(0.0);
                        view.changed();
                    })),
            )
    }

    fn live(&self, cx: &mut Context<Self>) -> Div {
        let source = self.session.audio.source;
        let idle = self.running.is_none() && !self.pending;
        let source_buttons = wrap().children(
            [
                InstrumentSource::Guitar,
                InstrumentSource::Bass,
                InstrumentSource::Microphone,
                InstrumentSource::MidiKeyboard,
                InstrumentSource::GuitarAndMidi,
                InstrumentSource::Playback,
            ]
            .into_iter()
            .map(|item| {
                button(
                    cx,
                    item.label(),
                    item.label(),
                    if idle {
                        state(source == item)
                    } else if item == source {
                        Style::Active
                    } else {
                        Style::Disabled
                    },
                    move |view| {
                        if view.running.is_none() && !view.pending {
                            view.session.audio.source = item;
                            view.changed();
                        }
                    },
                )
            }),
        );
        column()
            .gap_5()
            .child(title("Live", "Play, monitor and capture a take."))
            .child(card().child(heading("Source")).child(source_buttons))
            .child(
                card()
                    .child(heading("Current sound"))
                    .child(
                        row()
                            .child(div().text_2xl().child(if source.has_midi() {
                                self.session.audio.synth_sound.label().to_owned()
                            } else {
                                self.session.current_preset.clone()
                            }))
                            .child(div().flex_1())
                            .child(button(
                                cx,
                                "open-sounds",
                                "Sound library",
                                Style::Normal,
                                |view| view.page = Page::Library,
                            ))
                            .child(button(
                                cx,
                                "open-design",
                                "Edit effects",
                                Style::Normal,
                                |view| view.page = Page::Sound,
                            )),
                    )
                    .when(source.has_midi(), |panel| {
                        panel.child(muted(format!(
                            "{} voices · {} MIDI messages",
                            self.engine.midi.voices.load(Relaxed),
                            self.engine.midi.messages.load(Relaxed)
                        )))
                    }),
            )
            .child(self.backing(cx))
            .when(!self.last_take.is_empty(), |view| {
                view.child(
                    card()
                        .child(heading("Latest take"))
                        .children(self.last_take.iter().map(|path| muted(filename(path)))),
                )
            })
    }

    fn backing(&self, cx: &mut Context<Self>) -> Div {
        let playing = self.engine.media.playing.load(Relaxed);
        let length = self.engine.media.length.load(Relaxed);
        let position = self.engine.media.position.load(Relaxed);
        let seek = if length == 0 {
            0.0
        } else {
            position as f32 / length as f32
        };
        card()
            .child(heading("Backing track"))
            .child(muted(
                self.session
                    .backing_path
                    .as_deref()
                    .map_or("Choose an audio file".into(), filename),
            ))
            .when(!self.backing_wave.is_empty(), |card| {
                card.child(waveform(
                    cx,
                    "backing waveform",
                    &self.backing_wave,
                    seek,
                    true,
                ))
            })
            .child(
                wrap()
                    .child(button(
                        cx,
                        "backing-load",
                        "Import audio",
                        Style::Normal,
                        |view| view.choose_file(media::Role::Backing),
                    ))
                    .child(button(
                        cx,
                        "backing-play",
                        if playing { "Pause" } else { "Play" },
                        if length == 0 || self.pending {
                            Style::Disabled
                        } else {
                            state(playing)
                        },
                        |view| {
                            if view.engine.media.playing.load(Relaxed) {
                                view.engine.media.playing.store(false, Relaxed);
                            } else {
                                if view.engine.media.position.load(Relaxed)
                                    >= view.engine.media.length.load(Relaxed)
                                {
                                    view.engine.media.seek(0.0);
                                }
                                if view.running.is_some() {
                                    view.engine.media.playing.store(true, Relaxed);
                                } else {
                                    view.backing_next = true;
                                    view.start(InstrumentSource::Playback);
                                }
                            }
                        },
                    ))
                    .child(button(
                        cx,
                        "backing-rewind",
                        "Rewind",
                        Style::Normal,
                        |view| view.engine.media.seek(0.0),
                    ))
                    .child(button(
                        cx,
                        "backing-loop",
                        "Loop",
                        state(self.session.media.backing_loop),
                        |view| {
                            view.session.media.backing_loop = !view.session.media.backing_loop;
                            view.changed();
                        },
                    ))
                    .child(button(
                        cx,
                        "metronome",
                        "Metronome",
                        state(self.session.media.metronome),
                        |view| {
                            view.session.media.metronome = !view.session.media.metronome;
                            view.changed();
                        },
                    )),
            )
            .when(length > 0, |card| {
                card.child(control(
                    cx,
                    "Playback position",
                    format!("{:.0}%", seek * 100.0),
                    |view, delta| {
                        let length = view.engine.media.length.load(Relaxed).max(1);
                        view.engine.media.seek(
                            view.engine.media.position.load(Relaxed) as f32 / length as f32
                                + delta * 0.05,
                        );
                    },
                ))
            })
            .child(control(
                cx,
                "Backing level",
                format!("{:.0}%", self.session.media.backing_gain * 100.0),
                |view, delta| {
                    view.session.media.backing_gain =
                        (view.session.media.backing_gain + delta * 0.05).clamp(0.0, 1.0);
                    view.changed();
                },
            ))
            .child(control(
                cx,
                "Click tempo",
                format!("{:.0} BPM", self.session.media.bpm),
                |view, delta| {
                    view.session.media.bpm = (view.session.media.bpm + delta).clamp(30.0, 240.0);
                    view.changed();
                },
            ))
    }

    fn sound(&self, cx: &mut Context<Self>) -> Div {
        column()
            .gap_5()
            .child(title(
                "Sound design",
                "Shape the monitored signal while you play.",
            ))
            .child(
                card().child(heading("Amp & cabinet")).child(
                    wrap().children(
                        [
                            (Amp::Off, "Off"),
                            (Amp::Clean, "Clean combo"),
                            (Amp::Crunch, "Crunch stack"),
                            (Amp::Bass, "Bass cabinet"),
                        ]
                        .into_iter()
                        .map(|(amp, label)| {
                            button(
                                cx,
                                ("amp", amp as usize),
                                label,
                                state(self.session.parameters.effects.amp == amp),
                                move |view| {
                                    view.session.parameters.effects.amp = amp;
                                    view.changed();
                                },
                            )
                        }),
                    ),
                ),
            )
            .child(self.tone_controls(cx))
            .child(self.plugins(cx))
            .child(self.presets(cx, false))
    }

    fn plugins(&self, cx: &mut Context<Self>) -> Div {
        let stopped = !self.pending && self.running.is_none();
        let style = if stopped {
            Style::Normal
        } else {
            Style::Disabled
        };
        card()
            .child(row().justify_between().child(heading("CLAP insert"))
                .child(button(cx, "browse-clap", "Choose plugin…", style, |view| {
                    let send = view.send_job.clone();
                    std::thread::spawn(move || {
                        #[cfg(target_os = "macos")]
                        let path = rfd::FileDialog::new().set_title("Select a CLAP bundle").pick_folder();
                        #[cfg(not(target_os = "macos"))]
                        let path = rfd::FileDialog::new().add_filter("CLAP", &["clap"]).pick_file();
                        if let Some(path) = path { let _ = send.send(Job::ScanPlugin(path)); }
                    });
                })))
            .child(muted(self.plugin.as_ref().map_or_else(|| "Choose a trusted mono or stereo audio effect to insert after the built-in rack.".into(),
                |plugin| format!("{} · {}", plugin.name, plugin.path().display()))))
            .child(wrap()
                .child(button(cx, "disable-clap", "Off", if stopped { state(self.plugin.is_none()) } else { Style::Disabled }, |view| {
                    view.send(Command::SelectPlugin(None));
                }))
                .children(self.plugins.iter().cloned().enumerate().map(|(index, plugin)| {
                    button(cx, ("clap effect", index), plugin.name.clone(),
                        if stopped { state(self.plugin.as_ref() == Some(&plugin)) } else { Style::Disabled }, move |view| {
                        view.send(Command::SelectPlugin(Some(plugin.clone())));
                    })
                })))
    }

    fn tone_controls(&self, cx: &mut Context<Self>) -> Div {
        let p = self.session.parameters;
        card()
            .child(
                row()
                    .justify_between()
                    .child(heading("Effects"))
                    .child(button(cx, "bypass", "Bypass", state(p.bypass), |view| {
                        view.session.parameters.bypass ^= true;
                        view.changed();
                    })),
            )
            .child(control(
                cx,
                "Input trim",
                format!("{:.0} dB", p.input_trim_db),
                |v, d| {
                    v.session.parameters.input_trim_db =
                        (v.session.parameters.input_trim_db + d).clamp(-24.0, 24.0);
                    v.changed();
                },
            ))
            .child(control(
                cx,
                "Tone",
                format!("{:.0}%", p.tone * 100.0),
                |v, d| {
                    v.session.parameters.tone =
                        (v.session.parameters.tone + d * 0.05).clamp(0.0, 1.0);
                    v.changed();
                },
            ))
            .child(control(
                cx,
                "Drive",
                format!("{:.0}%", p.warmth * 100.0),
                |v, d| {
                    v.session.parameters.warmth =
                        (v.session.parameters.warmth + d * 0.05).clamp(0.0, 1.0);
                    v.changed();
                },
            ))
            .child(control(
                cx,
                "Delay",
                format!("{:.0}%", p.delay * 100.0),
                |v, d| {
                    v.session.parameters.delay =
                        (v.session.parameters.delay + d * 0.05).clamp(0.0, 1.0);
                    v.changed();
                },
            ))
            .child(control(
                cx,
                "Reverb",
                format!("{:.0}%", p.reverb * 100.0),
                |v, d| {
                    v.session.parameters.reverb =
                        (v.session.parameters.reverb + d * 0.05).clamp(0.0, 1.0);
                    v.changed();
                },
            ))
            .child(row().child(button(
                cx,
                "compressor",
                "Compressor",
                state(p.compression),
                |view| {
                    view.session.parameters.compression ^= true;
                    view.changed();
                },
            )))
    }

    fn vocals(&self, cx: &mut Context<Self>) -> Div {
        let p = self.session.parameters.effects;
        column()
            .gap_5()
            .child(title(
                "Vocals",
                "Monitor a microphone and audition pitch, color and harmony live.",
            ))
            .child(
                card()
                    .child(
                        row()
                            .justify_between()
                            .child(heading("Voice processor"))
                            .child(button(
                                cx,
                                "route-mic",
                                "Use microphone",
                                if self.running.is_some() || self.pending {
                                    Style::Disabled
                                } else {
                                    state(self.session.audio.source == InstrumentSource::Microphone)
                                },
                                |view| {
                                    view.session.audio.source = InstrumentSource::Microphone;
                                    view.changed();
                                },
                            )),
                    )
                    .child(control(
                        cx,
                        "Pitch shift",
                        format!("{:+.0} st", p.pitch),
                        |v, d| {
                            v.session.parameters.effects.pitch =
                                (v.session.parameters.effects.pitch + d).clamp(-12.0, 12.0);
                            v.changed();
                        },
                    ))
                    .child(control(
                        cx,
                        "Formant color",
                        format!("{:+.0}%", p.formant * 100.0),
                        |v, d| {
                            v.session.parameters.effects.formant =
                                (v.session.parameters.effects.formant + d * 0.1).clamp(-1.0, 1.0);
                            v.changed();
                        },
                    ))
                    .child(control(
                        cx,
                        "Harmony interval",
                        format!("{:+.0} st", p.harmony),
                        |v, d| {
                            v.session.parameters.effects.harmony =
                                (v.session.parameters.effects.harmony + d).clamp(-12.0, 12.0);
                            v.changed();
                        },
                    ))
                    .child(control(
                        cx,
                        "Harmony blend",
                        format!("{:.0}%", p.harmony_mix * 100.0),
                        |v, d| {
                            v.session.parameters.effects.harmony_mix =
                                (v.session.parameters.effects.harmony_mix + d * 0.05)
                                    .clamp(0.0, 0.65);
                            v.changed();
                        },
                    )),
            )
            .child(self.presets(cx, true))
            .child(self.tone_controls(cx))
    }

    fn presets(&self, cx: &mut Context<Self>, vocals: bool) -> Div {
        let mut presets = if vocals {
            [
                ("Natural voice", Effects::default()),
                (
                    "Octave down",
                    Effects {
                        pitch: -12.0,
                        formant: -0.2,
                        ..Default::default()
                    },
                ),
                (
                    "Fifth above",
                    Effects {
                        harmony: 7.0,
                        harmony_mix: 0.35,
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .map(|(name, effects)| Preset {
                name: name.into(),
                parameters: model::ToneParameters {
                    effects,
                    warmth: 0.0,
                    delay: 0.0,
                    reverb: 0.10,
                    tone: 0.5,
                    ..Default::default()
                },
            })
            .collect()
        } else {
            model::factory_presets()
        };
        presets.extend(self.session.user_presets.iter().cloned());
        let user_selected = self
            .session
            .user_presets
            .iter()
            .any(|p| p.name == self.session.current_preset);
        card()
            .child(heading(if vocals {
                "Vocal presets"
            } else {
                "Tone presets"
            }))
            .child(
                wrap().children(presets.into_iter().enumerate().map(|(index, preset)| {
                    button(
                        cx,
                        ("preset", index),
                        preset.name.clone(),
                        state(self.session.current_preset == preset.name),
                        move |view| view.load_preset(preset.clone()),
                    )
                })),
            )
            .child(
                row()
                    .child(button(
                        cx,
                        "save-preset",
                        "Save as new",
                        Style::Normal,
                        move |view| {
                            let prefix = if vocals {
                                "Vocal preset"
                            } else {
                                "Tone preset"
                            };
                            let name = (1..)
                                .map(|i| format!("{prefix} {i}"))
                                .find(|name| {
                                    view.session
                                        .user_presets
                                        .iter()
                                        .all(|preset| &preset.name != name)
                                })
                                .unwrap();
                            view.session.user_presets.push(Preset {
                                name: name.clone(),
                                parameters: view.session.parameters,
                            });
                            view.session.current_preset = name;
                            view.changed();
                        },
                    ))
                    .child(button(
                        cx,
                        "update-preset",
                        "Update",
                        if user_selected {
                            Style::Normal
                        } else {
                            Style::Disabled
                        },
                        |view| {
                            if let Some(preset) = view
                                .session
                                .user_presets
                                .iter_mut()
                                .find(|p| p.name == view.session.current_preset)
                            {
                                preset.parameters = view.session.parameters;
                                view.changed();
                            }
                        },
                    ))
                    .child(button(
                        cx,
                        "delete-preset",
                        "Delete",
                        if user_selected {
                            Style::Normal
                        } else {
                            Style::Disabled
                        },
                        |view| {
                            view.session
                                .user_presets
                                .retain(|p| p.name != view.session.current_preset);
                            view.session.current_preset = "Custom".into();
                            view.changed();
                        },
                    )),
            )
    }

    fn library(&self, cx: &mut Context<Self>) -> Div {
        let chosen = self.session.audio.synth_sound;
        let favorite = self.session.favorites.contains(&chosen);
        let stored = self
            .session
            .sample_path
            .as_deref()
            .map_or("Choose a sample".into(), filename);
        column()
            .gap_5()
            .child(title(
                "Sound library",
                "Browse sampled instruments, synthesizers and your own audio.",
            ))
            .child(
                card()
                    .child(
                        row().justify_between().child(heading("Instruments")).child(
                            row()
                                .child(button(
                                    cx,
                                    "audition",
                                    "Audition",
                                    Style::Primary,
                                    Workspace::audition,
                                ))
                                .child(button(
                                    cx,
                                    "panic",
                                    "All notes off",
                                    Style::Normal,
                                    |view| view.engine.midi.all_notes_off(),
                                )),
                        ),
                    )
                    .child(
                        row()
                            .child(button(
                                cx,
                                "favorite sound",
                                if favorite {
                                    "★ Pinned"
                                } else {
                                    "☆ Pin sound"
                                },
                                state(favorite),
                                move |view| {
                                    if favorite {
                                        view.session.favorites.retain(|sound| *sound != chosen);
                                    } else {
                                        view.session.favorites.push(chosen);
                                    }
                                    view.changed();
                                },
                            ))
                            .child(button(
                                cx,
                                "filter favorites",
                                "Pinned only",
                                state(self.favorites_only),
                                |view| {
                                    view.favorites_only ^= true;
                                },
                            )),
                    )
                    .child(
                        wrap().children(
                            SynthSound::ALL
                                .into_iter()
                                .filter(|sound| {
                                    !self.favorites_only || self.session.favorites.contains(sound)
                                })
                                .map(|sound| {
                                    button(
                                        cx,
                                        ("sound", sound.index() as usize),
                                        sound.label(),
                                        state(chosen == sound),
                                        move |view| view.set_sound(sound),
                                    )
                                }),
                        ),
                    )
                    .child(muted(chosen.description())),
            )
            .child(
                card()
                    .child(heading("Sampler & SoundFonts"))
                    .child(muted(stored))
                    .when(!self.sample_wave.is_empty(), |card| {
                        card.child(waveform(
                            cx,
                            "sample waveform",
                            &self.sample_wave,
                            1.0,
                            false,
                        ))
                    })
                    .child(
                        wrap()
                            .child(button(
                                cx,
                                "import-sample",
                                "Import keyboard sample",
                                Style::Normal,
                                |view| view.choose_file(media::Role::Sample),
                            ))
                            .child(button(
                                cx,
                                "import-font",
                                "Import SoundFont",
                                Style::Normal,
                                |view| view.choose_file(media::Role::SoundFont),
                            ))
                            .child(button(
                                cx,
                                "sample-loop",
                                "Loop while held",
                                state(self.session.media.sample_loop),
                                |view| {
                                    view.session.media.sample_loop ^= true;
                                    view.changed();
                                },
                            )),
                    )
                    .child(control(
                        cx,
                        "Sample root",
                        note_name(self.session.media.sample_root),
                        |v, d| {
                            v.session.media.sample_root =
                                (i16::from(v.session.media.sample_root) + d as i16).clamp(0, 127)
                                    as u8;
                            v.changed();
                        },
                    ))
                    .child(control(
                        cx,
                        "Sample in",
                        format!("{:.0}%", self.session.media.sample_start * 100.0),
                        |v, d| {
                            v.session.media.sample_start = (v.session.media.sample_start
                                + d * 0.01)
                                .clamp(0.0, (v.session.media.sample_end - 0.001).max(0.0));
                            v.changed();
                        },
                    ))
                    .child(control(
                        cx,
                        "Sample out",
                        format!("{:.0}%", self.session.media.sample_end * 100.0),
                        |v, d| {
                            v.session.media.sample_end = (v.session.media.sample_end + d * 0.01)
                                .clamp((v.session.media.sample_start + 0.001).min(1.0), 1.0);
                            v.changed();
                        },
                    )),
            )
            .when(!self.session.library.is_empty(), |page| {
                page.child(
                    card().child(heading("Imported audio")).children(
                        self.session
                            .library
                            .iter()
                            .cloned()
                            .enumerate()
                            .map(|(i, entry)| {
                                let path = entry.path;
                                let backing = path.clone();
                                row()
                                    .child(div().flex_1().child(filename(&path)))
                                    .child(button(
                                        cx,
                                        ("library backing", i),
                                        "Use as backing",
                                        Style::Normal,
                                        move |view| {
                                            view.importer
                                                .import(media::Role::Backing, backing.clone());
                                        },
                                    ))
                                    .child(button(
                                        cx,
                                        ("library sample", i),
                                        "Use as sample",
                                        Style::Normal,
                                        move |view| {
                                            view.importer.import(media::Role::Sample, path.clone());
                                        },
                                    ))
                            }),
                    ),
                )
            })
    }

    fn setup(&self, cx: &mut Context<Self>) -> Div {
        let config = &self.session.audio;
        let idle = self.running.is_none() && !self.pending;
        let selectable = |selected| {
            if idle {
                state(selected)
            } else {
                Style::Disabled
            }
        };
        let available_inputs = self
            .devices
            .iter()
            .filter(|d| d.backend == config.backend && d.input_channels > 0);
        let available_outputs = self
            .devices
            .iter()
            .filter(|d| d.backend == config.backend && d.output_channels >= 2);
        let channels = self
            .devices
            .iter()
            .find(|d| d.id == config.input_id)
            .map_or(1, |d| d.input_channels);
        let diagnostics = &self.engine.diagnostics;
        column().gap_5()
            .child(title("Audio & MIDI", "Choose the hardware that routes audio through Shredini."))
            .child(card().child(row().justify_between().child(heading("Driver"))
                .child(button(cx, "refresh-devices", "Refresh devices", selectable(false), |view| { view.send(Command::Refresh); })))
                .child(wrap().children(Backend::available().iter().copied().enumerate().map(|(i, backend)| {
                    button(cx, ("backend", i), backend.label(), selectable(config.backend == backend), move |view| {
                        view.session.audio.backend = backend;
                        view.session.audio.input_id.clear();
                        view.session.audio.output_id.clear();
                        view.session.audio.input_channel = 0;
                        view.pick_default_output();
                        view.changed();
                    })
                })))
                .child(wrap().child(muted("Requested buffer")).children([0_u32, 128, 256, 512].into_iter().map(|frames| {
                    button(cx, ("buffer", frames as usize), if frames == 0 { "Driver default".into() } else { format!("{frames} frames") },
                        selectable(config.buffer_frames == frames), move |view| {
                            view.session.audio.buffer_frames = frames;
                            view.changed();
                        })
                }))))
            .child(card().child(heading("Audio input"))
                .child(wrap().children(available_inputs.enumerate().map(|(i, device)| {
                    let id = device.id.clone();
                    button(cx, ("input", i), device.name.clone(), selectable(config.input_id == id), move |view| {
                        view.session.audio.input_id.clone_from(&id);
                        view.session.audio.input_channel = 0;
                        view.changed();
                    })
                })))
                .child(wrap().child(muted("Channel")).children((0..channels.min(64)).map(|channel| {
                    button(cx, ("channel", channel), (channel + 1).to_string(), selectable(config.input_channel == channel), move |view| {
                        view.session.audio.input_channel = channel;
                        view.changed();
                    })
                }))))
            .child(card().child(heading("Stereo output"))
                .child(wrap().children(available_outputs.enumerate().map(|(i, device)| {
                    let id = device.id.clone();
                    button(cx, ("output", i), device.name.clone(), selectable(config.output_id == id), move |view| {
                        view.session.audio.output_id.clone_from(&id);
                        view.changed();
                    })
                }))))
            .child(card().child(heading("MIDI input")).child(wrap()
                .child(button(cx, "no-keyboard", "No keyboard", selectable(config.midi_port.is_empty()), |view| {
                    view.session.audio.midi_port.clear(); view.changed();
                }))
                .children(self.ports.iter().enumerate().map(|(i, port)| {
                    let id = port.id.clone();
                    button(cx, ("midi", i), port.name.clone(), selectable(config.midi_port == id), move |view| {
                        view.session.audio.midi_port.clone_from(&id);
                        view.changed();
                    })
                }))))
            .when(!self.device_notice.is_empty(), |page| page.child(muted(self.device_notice.clone())))
            .when(!self.midi_notice.is_empty(), |page| page.child(muted(self.midi_notice.clone())))
            .when_some(self.running.as_ref(), |page, info| page.child(card().child(heading("Diagnostics"))
                .child(muted(format!("Output {} Hz · {} · {:?} frames", info.output_rate, info.output_format, info.output_buffer)))
                .child(muted(format!("Queue + converter {:.1} ms · {} underruns · {} overruns · {} driver xruns",
                    f32::from_bits(diagnostics.app_latency_ms.load(Relaxed)),
                    diagnostics.underruns.load(Relaxed), diagnostics.overruns.load(Relaxed), diagnostics.xruns.load(Relaxed))))))
    }
}

fn waveform(
    cx: &mut Context<Workspace>,
    id: &'static str,
    samples: &[f32],
    progress: f32,
    seek: bool,
) -> gpui::Stateful<Div> {
    let bins = samples.chunks(6);
    let count = bins.len() as f32;
    row()
        .id(id)
        .gap_px()
        .h(px(42.))
        .w_full()
        .rounded_md()
        .bg(rgb(BG))
        .px_2()
        .children(bins.enumerate().map(|(i, samples)| {
            let position = i as f32 / count;
            let peak = samples.iter().copied().fold(0.0_f32, f32::max);
            let bin = div()
                .id(i)
                .flex_1()
                .min_w_0()
                .h(px(42.))
                .flex()
                .items_center()
                .child(
                    div()
                        .w_full()
                        .h(px((peak.sqrt().clamp(0.0, 1.0) * 32.0).max(2.0)))
                        .rounded_sm()
                        .bg(rgb(if position < progress {
                            ACCENT
                        } else {
                            0x615e77
                        })),
                );
            if seek {
                bin.cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.engine.media.seek(position);
                        cx.notify();
                    }))
            } else {
                bin
            }
        }))
}

fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note % 12)],
        i16::from(note / 12) - 1
    )
}
