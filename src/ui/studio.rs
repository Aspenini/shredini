use super::*;
use shredini_studio::{Content, GainPoint, MAX_TRACKS, Note, Project, Region, Track};

pub(super) enum Job {
    Tracks(Vec<Track>, Option<usize>),
    Opened(Project, PathBuf),
    Saved(PathBuf),
    Rendered(Arc<media::Clip>, f64),
    Exported(PathBuf),
    Failed(String),
    Cancelled,
}

pub(super) struct Editor {
    project: Project,
    path: Option<PathBuf>,
    undo: Vec<Project>,
    selected: usize,
    region: usize,
    playhead: f64,
    pub(super) take_start: f64,
    first_bar: usize,
    lowest_key: u8,
    note_length: f64,
    note_velocity: u8,
    automation_db: f32,
    selected_note: Option<usize>,
    pub(super) busy: bool,
    pub(super) play_when_running: Option<Arc<media::Clip>>,
    mix_origin: f64,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            project: Project::default(),
            path: None,
            undo: Vec::new(),
            selected: 0,
            region: 0,
            playhead: 0.0,
            take_start: 0.0,
            first_bar: 0,
            lowest_key: 60,
            note_length: 0.25,
            note_velocity: 95,
            automation_db: -6.0,
            selected_note: None,
            busy: false,
            play_when_running: None,
            mix_origin: 0.0,
        }
    }
}

impl Editor {
    pub(super) fn restore() -> Self {
        let project = recovery_path()
            .and_then(|path| Project::load(&path).ok())
            .unwrap_or_default();
        Self {
            project,
            ..Default::default()
        }
    }

    fn checkpoint(&mut self) {
        if self.undo.len() >= 12 {
            self.undo.remove(0);
        }
        self.undo.push(self.project.clone());
    }

    fn edit_track(&mut self, update: impl FnOnce(&mut Track)) {
        if self.busy || self.selected >= self.project.tracks.len() {
            return;
        }
        self.checkpoint();
        update(&mut self.project.tracks[self.selected]);
    }

    fn edit_region(&mut self, update: impl FnOnce(&mut Region)) {
        let index = self.region;
        self.edit_track(|track| {
            if let Content::Audio(regions) = &mut track.content
                && let Some(region) = regions.get_mut(index)
            {
                update(region);
            }
        });
    }

    fn audio_range(&self, region: &Region) -> (f64, f64) {
        let start = region.start_beat;
        let seconds = (region
            .trim_end
            .or(region.duration)
            .unwrap_or(region.trim_start + 2.0)
            - region.trim_start)
            .max(0.0);
        (start, start + seconds * self.project.bpm / 60.0)
    }
}

fn recovery_path() -> Option<PathBuf> {
    storage::session_path().and_then(|path| {
        path.parent()
            .map(|parent| parent.join("Recovered Studio.shredini"))
    })
}

fn save_recovery(project: &Project) -> anyhow::Result<()> {
    if let Some(path) = recovery_path() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        project.save(&path)?;
    }
    Ok(())
}

fn load_regions(
    paths: Vec<PathBuf>,
    start_beat: f64,
    target: Option<usize>,
) -> anyhow::Result<Job> {
    let mut tracks = Vec::new();
    for path in paths {
        let extension = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension == "mid" || extension == "midi" {
            let mut midi = shredini_studio::import_midi(&path)?;
            for track in &mut midi {
                if let Content::Midi {
                    notes, controllers, ..
                } = &mut track.content
                {
                    for note in notes {
                        note.beat += start_beat;
                    }
                    for event in controllers {
                        event.beat += start_beat;
                    }
                }
            }
            tracks.extend(midi);
        } else {
            let path = path.canonicalize()?;
            let clip = media::decode(&path)?;
            tracks.push(Track::new(
                filename(&path),
                Content::Audio(vec![Region {
                    path,
                    start_beat,
                    trim_start: 0.0,
                    trim_end: None,
                    duration: Some(clip.duration()),
                }]),
            ));
        }
    }
    Ok(Job::Tracks(tracks, target))
}

impl Workspace {
    fn studio_job(&mut self, operation: impl FnOnce() -> anyhow::Result<Job> + Send + 'static) {
        if self.studio.busy {
            return;
        }
        self.studio.busy = true;
        let send = self.send_job.clone();
        std::thread::spawn(move || {
            let job = operation().unwrap_or_else(|error| Job::Failed(format!("{error:#}")));
            let _ = send.send(super::Job::Studio(job));
        });
    }

    pub(super) fn insert_take(&mut self, saved: SavedTake) {
        let mut tracks = Vec::new();
        for path in saved.paths {
            let name = if filename(&path).contains("Instrument") {
                "Instrument take"
            } else {
                "Input take"
            };
            let mut track = Track::new(
                name.into(),
                Content::Audio(vec![Region {
                    path,
                    start_beat: self.studio.take_start,
                    trim_start: 0.0,
                    trim_end: None,
                    duration: Some(saved.duration),
                }]),
            );
            track.tone = Some(saved.take.tone);
            tracks.push(track);
        }
        let scale = self.studio.project.bpm / (60.0 * f64::from(saved.take.rate));
        let start = self.studio.take_start;
        let end = start + saved.duration * self.studio.project.bpm / 60.0;
        let events = saved
            .take
            .midi
            .into_iter()
            .map(|(frame, message)| (start + frame as f64 * scale, message));
        let mut midi =
            shredini_studio::midi_track("MIDI take".into(), saved.take.sound, events, end);
        if matches!(&midi.content, Content::Midi { notes, .. } if !notes.is_empty()) {
            // The dry instrument stem already represents this performance.
            midi.mute = true;
            midi.tone = Some(saved.take.tone);
            tracks.push(midi);
        }
        if self.add_tracks(tracks) {
            self.page = Page::Studio;
            self.recover_studio();
        }
    }

    fn recover_studio(&self) {
        let project = self.studio.project.clone();
        std::thread::spawn(move || {
            let _ = save_recovery(&project);
        });
    }

    pub(super) fn save_recovery_on_close(&self) {
        if self.studio.path.is_some()
            || !self.studio.project.tracks.is_empty()
            || !self.studio.undo.is_empty()
        {
            let _ = save_recovery(&self.studio.project);
        }
    }

    pub(super) fn capture_anchor(&mut self) {
        self.studio.take_start = if self.engine.mix.playing.load(Relaxed) {
            self.studio.mix_origin
                + self.engine.mix.position.load(Relaxed) as f64 * self.studio.project.bpm
                    / (60.0 * f64::from(shredini_studio::RATE))
        } else {
            self.studio.playhead
        };
    }

    pub(super) fn import_dropped(&mut self, paths: Vec<PathBuf>) {
        let start = self.studio.playhead;
        self.studio_job(move || load_regions(paths, start, None));
    }

    fn add_tracks(&mut self, tracks: Vec<Track>) -> bool {
        let start = self.studio.project.tracks.len();
        if start + tracks.len() > MAX_TRACKS {
            self.notice = format!(
                "Studio supports {MAX_TRACKS} tracks. Audio files already saved are available to import."
            );
            return false;
        }
        self.studio.checkpoint();
        self.studio.project.tracks.extend(tracks);
        self.studio.selected = start;
        self.studio.selected_note = None;
        self.studio.region = 0;
        true
    }

    pub(super) fn apply_studio_job(&mut self, job: Job) {
        self.studio.busy = false;
        match job {
            Job::Tracks(tracks, target) => {
                if let Some(target) = target {
                    let regions = tracks.into_iter().flat_map(|track| match track.content {
                        Content::Audio(regions) => regions,
                        _ => Vec::new(),
                    });
                    if let Some(Track {
                        content: Content::Audio(_),
                        ..
                    }) = self.studio.project.tracks.get(target)
                    {
                        self.studio.selected = target;
                        self.studio.edit_track(move |track| {
                            if let Content::Audio(existing) = &mut track.content {
                                existing.extend(regions);
                            }
                        });
                    }
                } else {
                    self.add_tracks(tracks);
                }
            }
            Job::Opened(project, path) => {
                self.engine.mix.stop();
                self.studio = Editor {
                    project,
                    path: Some(path),
                    ..Default::default()
                };
            }
            Job::Saved(path) => {
                self.notice = format!("Project saved · {}", path.display());
                self.studio.path = Some(path);
            }
            Job::Rendered(clip, origin) => {
                self.studio.mix_origin = origin;
                if self.running.is_some() {
                    self.send(Command::PlayMix(clip));
                } else {
                    self.start(InstrumentSource::Playback);
                    if self.pending {
                        self.studio.play_when_running = Some(clip);
                    }
                }
            }
            Job::Exported(path) => self.notice = format!("Exported {}", path.display()),
            Job::Failed(error) => self.notice = error,
            Job::Cancelled => {}
        }
    }

    fn import_audio(&mut self, append: bool) {
        let start_beat = self.studio.playhead;
        let target = append.then_some(self.studio.selected);
        self.studio_job(move || {
            let Some(paths) = rfd::FileDialog::new()
                .add_filter("Audio", &["wav", "flac", "mp3", "ogg"])
                .pick_files()
            else {
                return Ok(Job::Cancelled);
            };
            load_regions(paths, start_beat, target)
        });
    }

    fn import_midi(&mut self) {
        let offset = self.studio.playhead;
        self.studio_job(move || {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("MIDI", &["mid", "midi"])
                .pick_file()
            else {
                return Ok(Job::Cancelled);
            };
            load_regions(vec![path], offset, None)
        });
    }

    fn save_project(&mut self, save_as: bool) {
        let project = self.studio.project.clone();
        let current = self.studio.path.clone();
        self.studio_job(move || {
            let path = if save_as || current.is_none() {
                rfd::FileDialog::new()
                    .add_filter("Shredini project", &["shredini"])
                    .set_file_name("Song.shredini")
                    .save_file()
            } else {
                current
            };
            let Some(path) = path else {
                return Ok(Job::Cancelled);
            };
            project.save(&path)?;
            Ok(Job::Saved(path))
        });
    }

    fn open_project(&mut self) {
        // Recovery keeps the previous arrangement available while switching projects.
        if !self.studio.project.tracks.is_empty() {
            self.recover_studio();
        }
        self.studio_job(|| {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Shredini project", &["shredini"])
                .pick_file()
            else {
                return Ok(Job::Cancelled);
            };
            Ok(Job::Opened(Project::load(&path)?, path))
        });
    }

    fn preview_mix(&mut self) {
        let project = self.studio.project.clone();
        let origin = self.studio.playhead;
        self.studio_job(move || {
            let mut clip = project.render()?;
            let frame = (origin * 60.0 / project.bpm * f64::from(clip.rate)).round() as usize;
            anyhow::ensure!(
                frame < clip.frames.len(),
                "The playhead is beyond the last audible track"
            );
            if frame > 0 {
                Arc::get_mut(&mut clip)
                    .expect("fresh mix")
                    .frames
                    .drain(..frame);
            }
            Ok(Job::Rendered(clip, origin))
        });
    }

    fn export_mix(&mut self) {
        let project = self.studio.project.clone();
        self.studio_job(move || {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("WAV", &["wav"])
                .set_file_name("Song.wav")
                .save_file()
            else {
                return Ok(Job::Cancelled);
            };
            let clip = project.render()?;
            shredini_studio::export_wav(
                &path,
                clip.rate,
                2,
                clip.frames.iter().flatten().copied(),
            )?;
            Ok(Job::Exported(path))
        });
    }

    pub(super) fn studio_page(&self, cx: &mut Context<Self>) -> Div {
        let editor = &self.studio;
        let ready = if editor.busy {
            Style::Disabled
        } else {
            Style::Normal
        };
        let replace_project = if self.recording
            || self.take_pending
            || self.take_saving
            || self.unsaved_take.is_some()
        {
            Style::Disabled
        } else {
            ready
        };
        let play = self.engine.mix.playing.load(Relaxed);
        let playing_beat = editor.mix_origin
            + self.engine.mix.position.load(Relaxed) as f64 / f64::from(shredini_studio::RATE)
                * editor.project.bpm
                / 60.0;
        let top = card()
            .child(
                wrap()
                    .child(button(cx, "new-project", "New", replace_project, |v| {
                        v.engine.mix.stop();
                        v.studio.checkpoint();
                        v.studio.project = Project::default();
                        v.studio.path = None;
                        v.studio.selected = 0;
                        v.studio.region = 0;
                        v.studio.playhead = 0.0;
                    }))
                    .child(button(
                        cx,
                        "open-project",
                        "Open",
                        replace_project,
                        Workspace::open_project,
                    ))
                    .child(button(cx, "save-project", "Save", ready, |v| {
                        v.save_project(false)
                    }))
                    .child(button(cx, "save-project-as", "Save as", ready, |v| {
                        v.save_project(true)
                    }))
                    .child(button(
                        cx,
                        "undo",
                        "Undo",
                        if editor.undo.is_empty() {
                            Style::Disabled
                        } else {
                            ready
                        },
                        |v| {
                            if let Some(previous) = v.studio.undo.pop() {
                                v.studio.project = previous;
                                v.studio.selected = v
                                    .studio
                                    .selected
                                    .min(v.studio.project.tracks.len().saturating_sub(1));
                                v.studio.region = 0;
                                v.studio.selected_note = None;
                            }
                        },
                    ))
                    .child(div().flex_1())
                    .child(button(
                        cx,
                        "play-mix",
                        if play { "Stop mix" } else { "Play mix" },
                        if play {
                            Style::Active
                        } else if editor.busy || self.pending || editor.project.tracks.is_empty() {
                            Style::Disabled
                        } else {
                            Style::Primary
                        },
                        |v| {
                            if v.engine.mix.playing.load(Relaxed) {
                                v.engine.mix.stop();
                            } else {
                                v.preview_mix();
                            }
                        },
                    ))
                    .child(button(
                        cx,
                        "export",
                        "Export WAV",
                        if editor.project.tracks.is_empty() {
                            Style::Disabled
                        } else {
                            ready
                        },
                        Workspace::export_mix,
                    )),
            )
            .child(
                row()
                    .justify_between()
                    .child(muted(
                        editor
                            .path
                            .as_deref()
                            .map_or("Untitled arrangement".into(), filename),
                    ))
                    .child(muted(if editor.busy {
                        "Working…".into()
                    } else if play {
                        format!("Playing · beat {:.2}", playing_beat + 1.0)
                    } else {
                        format!("Playhead · beat {:.2}", editor.playhead + 1.0)
                    })),
            )
            .child(control(
                cx,
                "Playhead position",
                format!("{:.2}", editor.playhead + 1.0),
                |v, d| {
                    v.studio.playhead =
                        (v.studio.playhead + f64::from(d) * 0.25).clamp(0.0, 2400.0);
                },
            ))
            .child(control(
                cx,
                "Project tempo",
                format!("{:.0} BPM", editor.project.bpm),
                |v, d| {
                    if !v.studio.busy {
                        v.studio.checkpoint();
                        v.studio.project.bpm =
                            (v.studio.project.bpm + f64::from(d)).clamp(30.0, 240.0);
                    }
                },
            ));
        let timeline = card()
            .child(
                row().justify_between().child(heading("Arrangement")).child(
                    wrap()
                        .child(button(cx, "import-region", "Import audio", ready, |v| {
                            v.import_audio(false)
                        }))
                        .child(button(
                            cx,
                            "import-midi",
                            "Import MIDI",
                            ready,
                            Workspace::import_midi,
                        ))
                        .child(button(cx, "create-midi", "Add MIDI track", ready, |v| {
                            let name = format!("MIDI {}", v.studio.project.tracks.len() + 1);
                            v.add_tracks(vec![Track::new(
                                name,
                                Content::Midi {
                                    sound: SynthSound::GrandPiano,
                                    notes: Vec::new(),
                                    controllers: Vec::new(),
                                },
                            )]);
                        })),
                ),
            )
            .child(
                row()
                    .child(div().w(px(128.)).flex_shrink_0().child("Track"))
                    .child(button(cx, "timeline-prev", "‹", Style::Normal, |v| {
                        v.studio.first_bar = v.studio.first_bar.saturating_sub(4)
                    }))
                    .children((0..8).map(|offset| {
                        let bar = editor.first_bar + offset;
                        let beat = (bar * 4) as f64;
                        div()
                            .id(("timeline ruler", bar))
                            .flex_1()
                            .min_w_0()
                            .py_1()
                            .text_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .bg(rgb(if editor.playhead == beat {
                                0x352c52
                            } else {
                                BG
                            }))
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.studio.playhead = beat;
                                cx.notify();
                            }))
                            .child((bar + 1).to_string())
                    }))
                    .child(button(cx, "timeline-next", "›", Style::Normal, |v| {
                        v.studio.first_bar = (v.studio.first_bar + 4).min(600)
                    })),
            )
            .children(
                editor
                    .project
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, track)| self.track_lane(cx, i, track)),
            )
            .when(editor.project.tracks.is_empty(), |card| {
                card.child(muted(
                    "Import a clip, create a MIDI track, or record a take in Live.",
                ))
            });
        column()
            .gap_5()
            .child(title(
                "Shredini Studio",
                "Arrange audio and MIDI, shape the mix, and export your song.",
            ))
            .child(top)
            .child(timeline)
            .when_some(editor.project.tracks.get(editor.selected), |page, track| {
                page.child(self.track_panel(cx, track))
            })
    }

    fn track_lane(&self, cx: &mut Context<Self>, index: usize, track: &Track) -> Div {
        let select = move |view: &mut Workspace| {
            if view.studio.selected != index {
                view.studio.selected = index;
                view.studio.selected_note = None;
                view.studio.region = 0;
            }
        };
        let editor = &self.studio;
        row()
            .child(
                div()
                    .w(px(128.))
                    .flex_shrink_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(button(
                        cx,
                        ("select track", index),
                        track.name.clone(),
                        state(editor.selected == index),
                        select,
                    )),
            )
            .child(div().w(px(32.)).flex_shrink_0())
            .children((0..8).map(|offset| {
                let bar = editor.first_bar + offset;
                let beat = (bar * 4) as f64;
                let (active, region) = match &track.content {
                    Content::Audio(regions) => {
                        let region = regions.iter().position(|region| {
                            let (start, end) = editor.audio_range(region);
                            beat < end && start < beat + 4.0
                        });
                        (region.is_some(), region)
                    }
                    Content::Midi { notes, .. } => (
                        notes
                            .iter()
                            .any(|n| beat < n.beat + n.duration && n.beat < beat + 4.0),
                        None,
                    ),
                };
                let color = match (&track.content, track.mute, active) {
                    (_, _, false) => BG,
                    (_, true, true) => LINE,
                    (Content::Audio(_), false, true) => 0x485d70,
                    (Content::Midi { .. }, false, true) => 0x695381,
                };
                div()
                    .id(("track bar", index * 8 + offset))
                    .flex_1()
                    .min_w_0()
                    .h(px(27.))
                    .rounded_sm()
                    .bg(rgb(color))
                    .cursor_pointer()
                    .hover(|s| s.border_1().border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        select(view);
                        view.studio.playhead = beat;
                        if let Some(region) = region {
                            view.studio.region = region;
                        }
                        cx.notify();
                    }))
            }))
            .child(div().w(px(32.)).flex_shrink_0())
    }

    fn track_panel(&self, cx: &mut Context<Self>, track: &Track) -> Div {
        let ready = if self.studio.busy {
            Style::Disabled
        } else {
            Style::Normal
        };
        let mixer = card()
            .child(
                row()
                    .justify_between()
                    .child(heading(track.name.clone()))
                    .child(
                        wrap()
                            .child(button(
                                cx,
                                "track-mute",
                                if track.mute { "Unmute" } else { "Mute" },
                                if track.mute && !self.studio.busy {
                                    Style::Active
                                } else {
                                    ready
                                },
                                |v| v.studio.edit_track(|track| track.mute ^= true),
                            ))
                            .child(button(cx, "remove-track", "Remove track", ready, |v| {
                                v.studio.checkpoint();
                                v.studio.project.tracks.remove(v.studio.selected);
                                v.studio.selected = v
                                    .studio
                                    .selected
                                    .min(v.studio.project.tracks.len().saturating_sub(1));
                                v.studio.selected_note = None;
                                v.studio.region = 0;
                            })),
                    ),
            )
            .child(control(
                cx,
                "Track gain",
                format!("{:+.0} dB", track.gain_db),
                |v, d| {
                    v.studio
                        .edit_track(|track| track.gain_db = (track.gain_db + d).clamp(-60.0, 12.0));
                },
            ))
            .child(control(
                cx,
                "Track pan",
                if track.pan == 0.0 {
                    "Center".into()
                } else {
                    format!(
                        "{:.0}% {}",
                        track.pan.abs() * 100.0,
                        if track.pan < 0.0 { "L" } else { "R" }
                    )
                },
                |v, d| {
                    v.studio.edit_track(|track| {
                        let pan = (track.pan + d * 0.1).clamp(-1.0, 1.0);
                        track.pan = if pan.abs() < 0.001 { 0.0 } else { pan };
                    })
                },
            ))
            .child(
                row()
                    .child(button(
                        cx,
                        "apply-live-tone",
                        "Copy current live effects",
                        ready,
                        |v| {
                            let tone = v.session.parameters;
                            v.studio.edit_track(|track| track.tone = Some(tone));
                        },
                    ))
                    .child(button(
                        cx,
                        "remove-live-tone",
                        "Clear effects",
                        if track.tone.is_some() {
                            ready
                        } else {
                            Style::Disabled
                        },
                        |v| {
                            v.studio.edit_track(|track| track.tone = None);
                        },
                    )),
            )
            .when_some(track.tone, |panel, tone| {
                panel
                    .child(control(
                        cx,
                        "Track drive",
                        format!("{:.0}%", tone.warmth * 100.0),
                        |v, d| {
                            v.studio.edit_track(|track| {
                                if let Some(p) = &mut track.tone {
                                    p.warmth = (p.warmth + d * 0.05).clamp(0.0, 1.0);
                                }
                            });
                        },
                    ))
                    .child(control(
                        cx,
                        "Track reverb",
                        format!("{:.0}%", tone.reverb * 100.0),
                        |v, d| {
                            v.studio.edit_track(|track| {
                                if let Some(p) = &mut track.tone {
                                    p.reverb = (p.reverb + d * 0.05).clamp(0.0, 1.0);
                                }
                            });
                        },
                    ))
                    .child(control(
                        cx,
                        "Track pitch",
                        format!("{:+.0} st", tone.effects.pitch),
                        |v, d| {
                            v.studio.edit_track(|track| {
                                if let Some(p) = &mut track.tone {
                                    p.effects.pitch = (p.effects.pitch + d).clamp(-12.0, 12.0);
                                }
                            });
                        },
                    ))
            });
        column()
            .gap_5()
            .child(mixer)
            .child(match &track.content {
                Content::Audio(regions) => self.audio_regions(cx, regions),
                Content::Midi { notes, sound, .. } => self.piano_roll(cx, notes, *sound),
            })
            .child(self.automation(cx, &track.gain_automation))
    }

    fn audio_regions(&self, cx: &mut Context<Self>, regions: &[Region]) -> Div {
        let ready = if self.studio.busy {
            Style::Disabled
        } else {
            Style::Normal
        };
        card()
            .child(
                row()
                    .justify_between()
                    .child(heading("Audio regions"))
                    .child(button(cx, "append-region", "Add to track", ready, |v| {
                        v.import_audio(true)
                    })),
            )
            .child(
                wrap().children(regions.iter().enumerate().map(|(i, region)| {
                    button(
                        cx,
                        ("audio region", i),
                        filename(&region.path),
                        state(i == self.studio.region),
                        move |v| v.studio.region = i,
                    )
                })),
            )
            .when_some(regions.get(self.studio.region), |card, region| {
                card.child(
                    row()
                        .child(button(cx, "move-region", "Move to playhead", ready, |v| {
                            let beat = v.studio.playhead;
                            v.studio.edit_region(|region| region.start_beat = beat);
                        }))
                        .child(button(
                            cx,
                            "duplicate-region",
                            "Duplicate to playhead",
                            ready,
                            |v| {
                                let index = v.studio.region;
                                let beat = v.studio.playhead;
                                v.studio.edit_track(|track| {
                                    if let Content::Audio(regions) = &mut track.content
                                        && let Some(region) = regions.get(index)
                                    {
                                        regions.push(Region {
                                            start_beat: beat,
                                            ..region.clone()
                                        });
                                    }
                                });
                            },
                        ))
                        .child(button(cx, "remove-region", "Remove region", ready, |v| {
                            let index = v.studio.region;
                            v.studio.edit_track(|track| {
                                if let Content::Audio(regions) = &mut track.content
                                    && index < regions.len()
                                {
                                    regions.remove(index);
                                }
                            });
                            v.studio.region = 0;
                        })),
                )
                .child(control(
                    cx,
                    "Region position",
                    format!("Beat {:.2}", region.start_beat + 1.0),
                    |v, d| {
                        v.studio.edit_region(|region| {
                            region.start_beat = (region.start_beat + f64::from(d) * 0.25).max(0.0)
                        });
                    },
                ))
                .child(control(
                    cx,
                    "Trim start",
                    format!("{:.2} s", region.trim_start),
                    |v, d| {
                        v.studio.edit_region(|region| {
                            let end = region.trim_end.or(region.duration).unwrap_or(600.0);
                            region.trim_start = (region.trim_start + f64::from(d) * 0.05)
                                .clamp(0.0, (end - 0.01).max(0.0));
                        });
                    },
                ))
                .child(control(
                    cx,
                    "Trim end",
                    region
                        .trim_end
                        .or(region.duration)
                        .map_or("End".into(), |end| format!("{end:.2} s")),
                    |v, d| {
                        v.studio.edit_region(|region| {
                            if let Some(end) = region.trim_end.or(region.duration) {
                                let duration = region.duration.unwrap_or(600.0);
                                let minimum = (region.trim_start + 0.01).min(duration);
                                region.trim_end =
                                    Some((end + f64::from(d) * 0.05).clamp(minimum, duration));
                            }
                        });
                    },
                ))
            })
    }

    fn piano_roll(&self, cx: &mut Context<Self>, notes: &[Note], sound: SynthSound) -> Div {
        let editor = &self.studio;
        let start = (editor.playhead / 4.0).floor() * 4.0;
        let mut hits = [[None; 16]; 12];
        let first = editor.lowest_key;
        for (index, note) in notes.iter().enumerate() {
            if !(first..first + 12).contains(&note.key)
                || note.beat >= start + 4.0
                || note.beat + note.duration <= start
            {
                continue;
            }
            let begin = ((note.beat - start) * 4.0).floor().max(0.0) as usize;
            let end = ((note.beat + note.duration - start) * 4.0)
                .ceil()
                .clamp(0.0, 16.0) as usize;
            for cell in &mut hits[usize::from(note.key - first)][begin.min(end)..end] {
                if cell.is_none() {
                    *cell = Some(index);
                }
            }
        }
        let mut grid = column()
            .gap_1()
            .child(
                row()
                    .gap_1()
                    .child(div().w(px(49.)))
                    .children((0..16).map(|step| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_center()
                            .text_color(rgb(MUTED))
                            .child(if step % 4 == 0 {
                                format!("{:.0}", start + f64::from(step / 4) + 1.0)
                            } else {
                                String::new()
                            })
                    })),
            );
        for key in (editor.lowest_key..editor.lowest_key + 12).rev() {
            grid = grid.child(
                row()
                    .gap_1()
                    .child(div().w(px(49.)).text_xs().child(note_name(key)))
                    .children((0_usize..16).map(|step| {
                        let beat = start + step as f64 * 0.25;
                        let note = hits[usize::from(key - first)][step];
                        div()
                            .id(("note grid", usize::from(key) * 16 + step))
                            .flex_1()
                            .min_w_0()
                            .h(px(18.))
                            .rounded_sm()
                            .cursor_pointer()
                            .bg(rgb(match note {
                                Some(index) if editor.selected_note == Some(index) => ACCENT,
                                Some(_) => 0x765da0,
                                None if step % 4 == 0 => LINE,
                                None => BG,
                            }))
                            .hover(|s| s.bg(rgb(0x574b72)))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(note) = note {
                                    view.studio.selected_note = Some(note);
                                } else if !view.studio.busy {
                                    let duration = view.studio.note_length;
                                    let velocity = view.studio.note_velocity;
                                    let mut selected = None;
                                    view.studio.edit_track(|track| {
                                        if let Content::Midi { notes, .. } = &mut track.content {
                                            selected = Some(notes.len());
                                            notes.push(Note {
                                                key,
                                                channel: 0,
                                                velocity,
                                                beat,
                                                duration,
                                            });
                                        }
                                    });
                                    view.studio.selected_note = selected;
                                }
                                cx.notify();
                            }))
                    })),
            );
        }
        let selected = editor.selected_note.and_then(|index| notes.get(index));
        card()
            .child(
                row().justify_between().child(heading("Piano roll")).child(
                    row()
                        .child(button(
                            cx,
                            "previous-octave",
                            "Octave −",
                            Style::Normal,
                            |v| v.studio.lowest_key = v.studio.lowest_key.saturating_sub(12),
                        ))
                        .child(button(cx, "next-octave", "Octave +", Style::Normal, |v| {
                            v.studio.lowest_key = (v.studio.lowest_key + 12).min(116)
                        }))
                        .child(button(cx, "previous-bar", "Bar ‹", Style::Normal, |v| {
                            v.studio.playhead = (v.studio.playhead - 4.0).max(0.0)
                        }))
                        .child(button(cx, "next-bar", "Bar ›", Style::Normal, |v| {
                            v.studio.playhead = (v.studio.playhead + 4.0).min(2400.0)
                        })),
                ),
            )
            .child(
                wrap().children(
                    SynthSound::ALL
                        .into_iter()
                        .filter(|sound| sound.program().is_some())
                        .map(|instrument| {
                            button(
                                cx,
                                ("track instrument", instrument.index()),
                                instrument.label(),
                                state(sound == instrument),
                                move |v| {
                                    v.studio.edit_track(|track| {
                                        if let Content::Midi { sound, .. } = &mut track.content {
                                            *sound = instrument;
                                        }
                                    });
                                },
                            )
                        }),
                ),
            )
            .child(grid)
            .child(control(
                cx,
                "Note length",
                format!(
                    "{:.2} beats",
                    selected.map_or(editor.note_length, |n| n.duration)
                ),
                |v, d| {
                    v.studio.note_length =
                        (v.studio.note_length + f64::from(d) * 0.25).clamp(0.25, 8.0);
                    let selected = v.studio.selected_note;
                    if let Some(index) = selected {
                        v.studio.edit_track(|track| {
                            if let Content::Midi { notes, .. } = &mut track.content
                                && let Some(note) = notes.get_mut(index)
                            {
                                note.duration =
                                    (note.duration + f64::from(d) * 0.25).clamp(0.0625, 32.0);
                            }
                        });
                    }
                },
            ))
            .child(control(
                cx,
                "Velocity",
                selected
                    .map_or(editor.note_velocity, |n| n.velocity)
                    .to_string(),
                |v, d| {
                    v.studio.note_velocity =
                        (i16::from(v.studio.note_velocity) + d as i16 * 5).clamp(1, 127) as u8;
                    let selected = v.studio.selected_note;
                    if let Some(index) = selected {
                        v.studio.edit_track(|track| {
                            if let Content::Midi { notes, .. } = &mut track.content
                                && let Some(note) = notes.get_mut(index)
                            {
                                note.velocity =
                                    (i16::from(note.velocity) + d as i16 * 5).clamp(1, 127) as u8;
                            }
                        });
                    }
                },
            ))
            .child(row().child(button(
                cx,
                "delete-note",
                "Delete note",
                if selected.is_none() || editor.busy {
                    Style::Disabled
                } else {
                    Style::Normal
                },
                |v| {
                    if let Some(index) = v.studio.selected_note.take() {
                        v.studio.edit_track(|track| {
                            if let Content::Midi { notes, .. } = &mut track.content
                                && index < notes.len()
                            {
                                notes.remove(index);
                            }
                        });
                    }
                },
            )))
    }

    fn automation(&self, cx: &mut Context<Self>, points: &[GainPoint]) -> Div {
        let editor = &self.studio;
        card()
            .child(
                row()
                    .justify_between()
                    .child(heading("Gain automation"))
                    .child(button(
                        cx,
                        "set-automation",
                        "Set point at playhead",
                        if editor.busy {
                            Style::Disabled
                        } else {
                            Style::Normal
                        },
                        |v| {
                            let (beat, gain_db) = (v.studio.playhead, v.studio.automation_db);
                            v.studio.edit_track(|track| {
                                track
                                    .gain_automation
                                    .retain(|point| (point.beat - beat).abs() > 0.0001);
                                track.gain_automation.push(GainPoint { beat, gain_db });
                                track
                                    .gain_automation
                                    .sort_by(|a, b| a.beat.total_cmp(&b.beat));
                            });
                        },
                    )),
            )
            .child(control(
                cx,
                "Automation gain",
                format!("{:+.0} dB", editor.automation_db),
                |v, d| {
                    v.studio.automation_db = (v.studio.automation_db + d).clamp(-60.0, 12.0);
                },
            ))
            .children(points.iter().enumerate().map(|(index, point)| {
                row()
                    .justify_between()
                    .child(muted(format!(
                        "Beat {:.2}  ·  {:+.0} dB",
                        point.beat + 1.0,
                        point.gain_db
                    )))
                    .child(button(
                        cx,
                        ("remove automation", index),
                        "Remove",
                        if editor.busy {
                            Style::Disabled
                        } else {
                            Style::Normal
                        },
                        move |v| {
                            v.studio.edit_track(|track| {
                                if index < track.gain_automation.len() {
                                    track.gain_automation.remove(index);
                                }
                            });
                        },
                    ))
            }))
    }
}
