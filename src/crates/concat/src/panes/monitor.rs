// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The monitor: the frame at the playhead, one request at a time.
//!
//! A playback that misses frames at the quality picked plays a tier
//! smaller until it stops; see [`Strain`].
//!
//! The pane owns the picture, the busy and wanted flags that serialise
//! requests, the quality tier per timeline, and the worker that draws a
//! frame. What goes into the frame - the flattened clips with their
//! titles, a look being auditioned, a cutout being painted - is the
//! controller's to say, and the pane asks it for that list. A request
//! while one is out waits for it, and the newest wins; a frame that
//! could not be drawn says so once per project and then stops repeating
//! itself.
//!
//! The stage's gestures - pressing, dragging and releasing a picture on
//! the monitor - are not here: one [`crate::studio::Gesture`] spans the
//! stage and the lanes, so they live with the timeline's.

use std::collections::HashMap;
use std::sync::Arc;

use concat_host::preview::FrameSpec;
use concat_project::model::Project;

use crate::host::{spawn_detached, spawn_frame};
use crate::i18n::tf;
use crate::panes::Msg;
use crate::studio::Studio;

/// Everything that can happen to the monitor.
pub enum MonitorMsg {
    /// The frame at the playhead is wanted.
    Request,
    /// The worker is done: a frame, or why not.
    Frame(Result<Picture, String>, FrameSpec),
    /// The quality picker: 0 full, 1 half, 2 quarter.
    QualityChanged(i32),
    /// A project opened: the monitor starts clean and may complain again.
    Opened,
    /// The project closed: nothing to show.
    Closed,
}

impl std::fmt::Debug for MonitorMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request => write!(f, "Request"),
            Self::Frame(result, spec) => write!(
                f,
                "Frame({}, {spec:?})",
                match result {
                    Ok(Picture::Sources(_)) => "sources".to_owned(),
                    Ok(Picture::Pixels(_, w, h)) => format!("{w}x{h} pixels"),
                    Err(error) => format!("error {error:?}"),
                }
            ),
            Self::QualityChanged(index) => write!(f, "QualityChanged({index})"),
            Self::Opened => write!(f, "Opened"),
            Self::Closed => write!(f, "Closed"),
        }
    }
}

/// A monitor frame on its way to the window.
pub enum Picture {
    /// Decoded and placed, waiting to be drawn on the window's own device -
    /// which happens back on this thread, never on the worker that decoded
    /// it. See `Monitor::texture_of`.
    Sources(concat_export::PreviewSources),
    /// Raw RGBA, to be uploaded.
    Pixels(Vec<u8>, u32, u32),
}

/// The monitor's state.
#[derive(Default)]
pub struct MonitorPane {
    /// The last frame drawn.
    pub image: slint::Image,
    busy: bool,
    wanted: bool,
    /// Said once per project: a monitor that cannot decode says so, and
    /// then stops repeating itself.
    failed: bool,
    /// 0 Full, 1 Half, 2 Quarter of the output size, by timeline id. Kept
    /// per timeline because the cost it trades against is the timeline's
    /// frame - a 4K cut wants the quarter setting that a 1080p cut beside
    /// it does not - and the trade is the window's, not the document's,
    /// so it is remembered here and not saved.
    quality: HashMap<String, usize>,
    /// When the request out now was sent.
    asked: Option<std::time::Instant>,
    /// How the frames of the current playback arrived.
    cadence: Cadence,
    /// Whether the current playback keeps up, and how far below the chosen
    /// quality it has stepped because it did not.
    strain: Strain,
}

/// How far a playback that cannot keep up drops below the chosen quality.
///
/// Every second of timeline frames is a verdict: when more than a tenth of
/// them were never shown, the machine is not keeping up at this size, and
/// the next frames are asked for a tier smaller - Half to Quarter, Full to
/// Half - which is a quarter of the pixels to decode, upload and run every
/// effect over. Once, and only for this playback: it does not climb back
/// while playing, which would make it hunt between the two, and it is
/// forgotten when playback stops, so the paused picture is always the
/// quality picked. The pick itself is never touched.
#[derive(Default, Debug, PartialEq)]
struct Strain {
    /// Frames shown in the second being judged.
    shown: u32,
    /// Frames of that second that passed without being shown.
    skipped: u32,
    last: Option<i64>,
    /// Tiers below the chosen one, 0 or 1.
    step: usize,
}

impl Strain {
    /// The share of a second's frames that may go unshown before the
    /// quality steps down.
    const MISSED: f64 = 0.1;

    /// Counts a shown frame of timeline frame `index` at `fps`. True when
    /// this frame stepped the quality down.
    fn count(&mut self, index: i64, fps: f64) -> bool {
        self.shown += 1;
        if let Some(last) = self.last
            && index > last + 1
        {
            self.skipped += u32::try_from(index - last - 1).unwrap_or(u32::MAX);
        }
        self.last = Some(index);
        let second = fps.max(1.0).ceil() as u32;
        if self.shown + self.skipped < second {
            return false;
        }
        let missed = f64::from(self.skipped) / f64::from(self.shown + self.skipped);
        self.shown = 0;
        self.skipped = 0;
        if self.step == 0 && missed > Self::MISSED {
            self.step = 1;
            return true;
        }
        false
    }

    /// The tier to ask for when `chosen` was picked: one smaller while
    /// stepped down, never past Quarter.
    fn tier(&self, chosen: usize) -> usize {
        (chosen + self.step).min(2)
    }
}

/// How smoothly a playback's frames reached the monitor: one line in the
/// log when it stops, so a stutter is a number and not an impression.
#[derive(Default, Debug, PartialEq)]
struct Cadence {
    shown: u32,
    /// Frames showing the same timeline frame as the one before.
    repeated: u32,
    /// Timeline frames that passed without ever being shown.
    skipped: u32,
    /// The longest a request waited for its frame.
    worst: std::time::Duration,
    last: Option<i64>,
}

impl Cadence {
    /// Counts a frame of the timeline frame `index` that took `wait`.
    fn count(&mut self, index: i64, wait: std::time::Duration) {
        self.shown += 1;
        self.worst = self.worst.max(wait);
        match self.last {
            Some(last) if index == last => self.repeated += 1,
            Some(last) if index > last + 1 => {
                self.skipped += u32::try_from(index - last - 1).unwrap_or(u32::MAX);
            }
            _ => {}
        }
        self.last = Some(index);
    }

    /// Logs what was counted and starts over.
    fn report(&mut self, fps: f64) {
        if self.shown > 0 {
            log::info!(
                "playback: {} frames shown at {fps:.3} fps, {} repeated, {} skipped, \
                 worst wait {} ms",
                self.shown,
                self.repeated,
                self.skipped,
                self.worst.as_millis(),
            );
        }
        *self = Self::default();
    }
}

impl MonitorPane {
    /// Applies one message. The studio is the rest of the window; while
    /// this runs the studio's copy of the pane is a blank it must not read.
    pub fn update(&mut self, msg: MonitorMsg, studio: &mut Studio) {
        match msg {
            MonitorMsg::Request => self.request(studio),
            MonitorMsg::Frame(result, spec) => {
                self.busy = false;
                let fps = studio.project().active().video.rate();
                let wait = self.asked.take().map(|at| at.elapsed()).unwrap_or_default();
                if spec.moving && result.is_ok() {
                    let index = (spec.time * fps + 1e-6).floor() as i64;
                    self.cadence.count(index, wait);
                    if self.strain.count(index, fps) {
                        log::info!(
                            "playback: frames are being missed at {}x{}; playing a tier \
                             smaller until it stops",
                            spec.width,
                            spec.height
                        );
                    }
                } else if !spec.moving {
                    self.cadence.report(fps);
                    self.strain = Strain::default();
                }
                let picture = match result {
                    // Drawn here and not on the worker: this is the event
                    // loop, the one thread the window's renderer submits
                    // from, and a second thread submitting beside it hangs
                    // the GPU - see `Monitor::texture_of`.
                    Ok(Picture::Sources(sources)) => {
                        let drawing = std::time::Instant::now();
                        let drawn = studio.host.monitor.texture_of(&sources, spec);
                        // With CONCAT_LOG=debug, where each frame's time
                        // went: gathering it on the worker (decoding,
                        // compiling its new effects) against drawing it
                        // here, which is what a stutter is measured with.
                        log::debug!(
                            "preview frame {}x{} at {:.3}s: gathered in {} ms, drawn in {} ms",
                            spec.width,
                            spec.height,
                            spec.time,
                            wait.as_millis(),
                            drawing.elapsed().as_millis()
                        );
                        drawn.and_then(|texture| {
                            slint::Image::try_from(texture)
                                .map_err(|error| format!("preview texture: {error}"))
                        })
                    }
                    Ok(Picture::Pixels(bytes, width, height)) => {
                        let buffer =
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &bytes, width, height,
                            );
                        Ok(slint::Image::from_rgba8(buffer))
                    }
                    Err(error) => Err(error),
                };
                match picture {
                    Ok(image) => self.image = image,
                    Err(error) => {
                        log::warn!("preview: {error}");
                        if !self.failed {
                            self.failed = true;
                            studio.notify(&tf("monitor.previewFailed", &[&error]), true);
                        }
                    }
                }
                if self.wanted {
                    self.request(studio);
                }
            }
            MonitorMsg::QualityChanged(index) => {
                let id = studio.project().active_timeline_id.clone();
                self.quality.insert(id, (index.max(0) as usize).min(2));
                self.request(studio);
            }
            MonitorMsg::Opened => self.failed = false,
            MonitorMsg::Closed => self.image = slint::Image::default(),
        }
    }

    /// The quality tier for the project's active timeline: 0 full, 1
    /// half, 2 quarter. Half until chosen.
    pub fn quality_of(&self, project: &Project) -> usize {
        self.quality
            .get(&project.active_timeline_id)
            .copied()
            .unwrap_or(1)
    }

    /// The frame's size at a quality tier: the output scaled, rounded to
    /// even dimensions, never smaller than two pixels a side.
    pub fn frame_size(quality: usize, (width, height): (u32, u32)) -> (u32, u32) {
        let scale = match quality {
            0 => 1.0,
            1 => 0.5,
            _ => 0.25,
        };
        let side = |px: u32| ((f64::from(px) * scale).round() as u32).max(2) & !1;
        (side(width), side(height))
    }

    /// Asks the engine for the frame at the playhead - or under the pointer,
    /// while the preview axis has it - one at a time.
    fn request(&mut self, studio: &mut Studio) {
        if studio.on_start || studio.session.is_none() {
            return;
        }
        if self.busy {
            self.wanted = true;
            return;
        }
        let moving = studio.playing || studio.source.as_ref().is_some_and(|source| source.playing);
        if !moving {
            self.strain = Strain::default();
        }
        // The tier picked, or one smaller while this playback cannot keep
        // up at it; see `Strain`.
        let quality = self.strain.tier(self.quality_of(studio.project()));
        let (width, height) = Self::frame_size(quality, studio.output_size());
        let Some((clips, settings)) = studio.preview_clips() else {
            return;
        };
        let spec = FrameSpec {
            time: f64::from(studio.preview_time()),
            width,
            height,
            moving,
            // Playback reads the proxies at Half and Quarter; Full plays
            // the files themselves, or Full would be the proxy blurred up.
            // With proxies off in Settings it plays the files at every
            // quality, a proxy left from before included.
            proxy: studio.prefs.proxies && moving && quality > 0,
            color_space: studio.project().active().video.color_space,
        };
        if !studio.playing {
            self.cadence.report(studio.project().active().video.rate());
        }
        let monitor = studio.host.monitor.clone();
        self.busy = true;
        self.wanted = false;
        self.asked = Some(std::time::Instant::now());
        spawn_frame(
            move || {
                // On the window's device the frame stays a texture; without
                // one it comes back as pixels and is uploaded here.
                let frame = if monitor.has_gpu() {
                    monitor
                        .frame_sources(Arc::clone(&clips), &settings, spec)
                        .map(Picture::Sources)
                } else {
                    monitor
                        .frame(Arc::clone(&clips), &settings, spec)
                        .map(|bytes| Picture::Pixels(bytes, width, height))
                };
                // Decode-ahead for whatever comes next, on a worker of its
                // own, so the frame goes to the window without waiting for
                // it and the next frame can start meanwhile. Playing, the
                // full window ahead: the scheduler adds only the frames it
                // has not queued yet, so asking for all of them each frame
                // costs nothing and keeps a slow decode from catching up
                // with the playhead.
                {
                    let monitor = monitor.clone();
                    let settings = settings.clone();
                    let ahead = if spec.moving {
                        concat_media::prefetch::AHEAD
                    } else {
                        2
                    };
                    spawn_detached(move || monitor.prefetch(clips, &settings, spec, ahead));
                }
                frame
            },
            move |studio, _, _, result| {
                studio.handle(Msg::Monitor(MonitorMsg::Frame(result, spec)));
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_even_sided_and_never_vanishes() {
        assert_eq!(MonitorPane::frame_size(0, (1920, 1080)), (1920, 1080));
        assert_eq!(MonitorPane::frame_size(1, (1920, 1080)), (960, 540));
        assert_eq!(MonitorPane::frame_size(2, (1920, 1080)), (480, 270));
        // An odd output at half is rounded to even, as the encoder and the
        // chroma planes need.
        assert_eq!(MonitorPane::frame_size(1, (1001, 1001)), (500, 500));
        // Nothing gets smaller than two pixels a side, whatever the tier.
        assert_eq!(MonitorPane::frame_size(2, (1, 1)), (2, 2));
        assert_eq!(MonitorPane::frame_size(2, (0, 0)), (2, 2));
        // A tier past the picker's three is the smallest one.
        assert_eq!(MonitorPane::frame_size(9, (400, 400)), (100, 100));
    }

    #[test]
    fn a_cadence_counts_repeats_and_gaps() {
        let ms = std::time::Duration::from_millis;
        let mut cadence = Cadence::default();
        for (index, wait) in [(0, 10), (1, 12), (1, 40), (4, 8)] {
            cadence.count(index, ms(wait));
        }
        assert_eq!(cadence.shown, 4);
        assert_eq!(cadence.repeated, 1);
        assert_eq!(cadence.skipped, 2, "frames 2 and 3 never came");
        assert_eq!(cadence.worst, ms(40));
        cadence.report(30.0);
        assert_eq!(cadence, Cadence::default(), "a report starts over");
    }

    /// A second with more than a tenth of its frames missed steps the
    /// quality down once; a smooth one does not; it never steps twice or
    /// past Quarter.
    #[test]
    fn a_playback_that_misses_frames_plays_a_tier_smaller() {
        let mut smooth = Strain::default();
        assert!((0..60).all(|index| !smooth.count(index, 30.0)));
        assert_eq!(smooth.tier(1), 1);

        // Every third frame shown: two of three missed.
        let mut strained = Strain::default();
        let stepped: Vec<bool> = (0..12).map(|n| strained.count(n * 3, 30.0)).collect();
        assert_eq!(
            stepped.iter().filter(|&&step| step).count(),
            1,
            "{stepped:?}"
        );
        assert_eq!(strained.tier(0), 1, "Full plays at Half");
        assert_eq!(strained.tier(1), 2, "Half plays at Quarter");
        assert_eq!(strained.tier(2), 2, "and Quarter stays Quarter");
        assert!(
            (12..40).all(|n| !strained.count(n * 3, 30.0)),
            "it steps down once, not again"
        );

        // A frame or two missed in a second is within what is allowed.
        let mut nearly = Strain::default();
        let shown = (0..30).filter(|index| index % 15 != 7);
        assert!(
            shown
                .map(|index| nearly.count(index, 30.0))
                .all(|step| !step)
        );
        assert_eq!(nearly.step, 0);
    }

    #[test]
    fn quality_is_half_until_chosen_and_kept_per_timeline() {
        let mut pane = MonitorPane::default();
        let mut project = Project {
            active_timeline_id: "a".to_owned(),
            ..Project::default()
        };
        assert_eq!(pane.quality_of(&project), 1);
        pane.quality.insert("a".to_owned(), 2);
        assert_eq!(pane.quality_of(&project), 2);
        project.active_timeline_id = "b".to_owned();
        assert_eq!(pane.quality_of(&project), 1, "another timeline has its own");
    }
}
