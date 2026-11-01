// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The paused monitor's true frame.
//!
//! One reader pool for the app's lifetime: its whole value is what stays
//! warm between scrubs. The pool locks per reader, so a frame is decoded on
//! whatever thread the caller chose while another thread decodes ahead on a
//! different file - the window debounces and drops stale results, so a slow
//! decode never wedges anything but itself.
//!
//! With the `gpu` feature and a device from [`Monitor::with_gpu`], a frame
//! is drawn on that device and handed back as a texture: decoded pictures
//! go up once, the composite happens where it is shown, and no pixel comes
//! back down. A frame is therefore two calls and not one -
//! [`Monitor::frame_sources`] anywhere, [`Monitor::texture_of`] on the
//! thread that owns the device - because the drawing is not a thing a
//! worker may do; see `texture_of`.

use std::sync::{Arc, Mutex};

use concat_export::ExportClip;
use concat_project::DocumentSettings;
use concat_project::model::ColorSpace;

/// How far ahead of the playhead playback opens what a cut leads to; see
/// [`Monitor::prefetch`]. Three seconds, so a cold open has finished by the
/// cut: a 4K web download keeps a keyframe every few seconds, the first
/// frame after a seek decodes from the one before it, and on an integrated
/// GPU that took two and a half seconds. And one second, so a cut that
/// playback started closer to than three is still opened before it.
const NEXT_CUTS: [f64; 2] = [1.0, 3.0];

/// Whether two reads are on one reader: the same take of the same file
/// (see `FrameRequest::of_take`). The incoming side of a transition across
/// a cut from a file to itself is a take of its own, and is opened early
/// on a reader of its own, as a file not read yet is.
fn same_take(a: &concat_media::FrameRequest, b: &concat_media::FrameRequest) -> bool {
    a.path == b.path && a.take == b.take
}

/// The first read of the take `found` at, found at the instant `at`:
/// walked back a frame at a time while the instant before still reads
/// it, never to `now` or before. A cut seen three seconds ahead is seen
/// up to three seconds into it; opened there, its reader would be past
/// every read the cut begins with, and each of those would seek it back.
fn first_read(
    plan: &concat_export::PreviewPlan,
    now: f64,
    at: f64,
    found: concat_media::FrameRequest,
    fps: f64,
    proxy: bool,
) -> concat_media::FrameRequest {
    let frame = 1.0 / fps;
    let (mut at, mut first) = (at, found);
    while at - frame > now {
        at -= frame;
        let earlier = concat_export::preview_moments(plan, at - frame, 1, proxy)
            .pop()
            .and_then(|moment| {
                moment
                    .frames
                    .into_iter()
                    .find(|read| same_take(read, &first))
            });
        match earlier {
            Some(read) => first = read,
            None => break,
        }
    }
    first
}

/// A frame request: the instant and the size, with the clips coming from
/// the session that owns them.
#[derive(Clone, Copy, Debug)]
pub struct FrameSpec {
    /// The timeline instant to composite, in seconds.
    pub time: f64,
    /// Preview frame width in pixels.
    pub width: u32,
    /// Preview frame height in pixels.
    pub height: u32,
    /// The picture is moving - playing - rather than paused or scrubbed:
    /// what tells the scheduler which way to decode ahead.
    pub moving: bool,
    /// Read each file's proxy where it has one - the low stand-in that
    /// keeps playback smooth - rather than the file. A moving picture at
    /// less than full quality; at Full the file itself plays, since a
    /// proxy drawn at full size is a blur, which is what Full was picked
    /// against.
    pub proxy: bool,
    /// What the timeline is output in: an HDR one keeps its clips' light
    /// above white and is rolled off for the SDR screen.
    pub color_space: ColorSpace,
}

/// The reader pool behind the monitor, shareable across threads.
#[derive(Clone)]
pub struct Monitor {
    pool: Arc<concat_media::ReaderPool>,
    /// The last clip list's plan, kept until the list changes: playback
    /// and scrubbing ask for many instants of one document, and the plan
    /// is the half of a frame that does not depend on the instant.
    plan: Arc<Mutex<Option<PlanEntry>>>,
    /// The takes a coming cut leads into that playback has already sent to
    /// be opened early, so each is opened once and not once a frame.
    early: Arc<Mutex<Vec<concat_media::FrameRequest>>>,
    #[cfg(feature = "gpu")]
    gpu: Option<Arc<Mutex<concat_render::WgpuCompositor>>>,
    /// A sibling of `gpu` that only ever compiles: each frame's new
    /// effects are compiled on it on the worker that gathers the frame
    /// (see [`Monitor::frame_sources`]), into the pipeline cache both
    /// share, so the event loop that draws the frame does not stall on the
    /// driver's compile the first time an effect is shown. Only where there
    /// is such a cache; without one the compile would happen twice.
    #[cfg(feature = "gpu")]
    warm: Option<Arc<Mutex<concat_render::WgpuCompositor>>>,
}

/// One kept plan and what it was built for.
struct PlanEntry {
    clips: Arc<Vec<ExportClip>>,
    width: u32,
    height: u32,
    rate: (i64, i64),
    color_space: ColorSpace,
    plan: Arc<concat_export::PreviewPlan>,
}

/// The wgpu the monitor's textures belong to.
#[cfg(feature = "gpu")]
pub use concat_render::wgpu;

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    /// A monitor with the engine's default pool budget.
    pub fn new() -> Self {
        Self {
            pool: Arc::clone(crate::scheduler().pool()),
            plan: Arc::new(Mutex::new(None)),
            early: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "gpu")]
            gpu: None,
            #[cfg(feature = "gpu")]
            warm: None,
        }
    }

    /// A monitor that composites on `device` - the window's - so
    /// [`Monitor::texture_of`] yields textures the window shows as they
    /// are.
    #[cfg(feature = "gpu")]
    pub fn with_gpu(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let compositor = concat_render::WgpuCompositor::with_device(device, queue);
        let warm = compositor
            .has_pipeline_cache()
            .then(|| Arc::new(Mutex::new(compositor.sibling())));
        Self {
            pool: Arc::clone(crate::scheduler().pool()),
            plan: Arc::new(Mutex::new(None)),
            early: Arc::new(Mutex::new(Vec::new())),
            gpu: Some(Arc::new(Mutex::new(compositor))),
            warm,
        }
    }

    /// Lets the GPU's frame textures go down to a quarter of their budget,
    /// for when playback stops: the pool fills as frames stream past, and a
    /// still picture needs only the few it shows. Nothing without a GPU.
    pub fn shrink(&self) {
        #[cfg(feature = "gpu")]
        if let Some(gpu) = self.gpu.as_ref() {
            gpu.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .shrink_pool(0.25);
        }
    }

    /// A compositor of its own on the window's device, for work off the
    /// monitor's thread - the effect cards. None without a GPU here.
    #[cfg(feature = "gpu")]
    pub fn sibling(&self) -> Option<concat_render::WgpuCompositor> {
        let gpu = self.gpu.as_ref()?;
        let gpu = gpu.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(gpu.sibling())
    }

    /// [`Monitor::sibling`] for work on a thread of its own - the export,
    /// the effect cards - where the window's queue can take it, and None
    /// on Vulkan, where the work then opens a device of its own. Skia
    /// submits the window's frames to the raw Vulkan queue without wgpu's
    /// lock, and a Vulkan queue must not be submitted to from two threads
    /// at once: an export from the window on Linux lost the device part
    /// way through, and the window's with it (#324). Direct3D 12 and Metal
    /// queues take submissions from any thread, so there the work stays on
    /// the window's device, which is what kept the export alive on Windows
    /// laptops with NVIDIA chips (#202).
    #[cfg(feature = "gpu")]
    pub fn off_thread(&self) -> Option<concat_render::WgpuCompositor> {
        let gpu = self.gpu.as_ref()?;
        let gpu = gpu.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (gpu.adapter_info().backend != wgpu::Backend::Vulkan).then(|| gpu.sibling())
    }

    /// Whether frames can be composited on the GPU.
    pub fn has_gpu(&self) -> bool {
        #[cfg(feature = "gpu")]
        {
            self.gpu.is_some()
        }
        #[cfg(not(feature = "gpu"))]
        {
            false
        }
    }

    /// Makes `frame` the still the pool serves under `path`, a name no
    /// file has; see `ReaderPool::hold_still`.
    pub fn hold_still(&self, path: &std::path::Path, frame: Arc<concat_core::frame::Frame>) {
        self.pool.hold_still(path, frame);
    }

    /// The pictures a monitor frame is made of, decoded and placed but not
    /// yet drawn.
    ///
    /// The half of a frame that is safe on any thread: it reads files and
    /// the reader pool and never touches the device. The other half is
    /// [`Monitor::texture_of`], which is safe on exactly one - see there
    /// for why the two are split at all.
    #[cfg(feature = "gpu")]
    pub fn frame_sources(
        &self,
        clips: Arc<Vec<ExportClip>>,
        settings: &DocumentSettings,
        spec: FrameSpec,
    ) -> Result<concat_export::PreviewSources, String> {
        let plan = self.plan_for(clips, settings, spec);
        let sources = concat_export::preview_sources_of(&self.pool, &plan, spec.time, spec.proxy)?;
        // Compiling is not submitting: it never touches the queue the
        // window's renderer submits to, so unlike the drawing it is safe
        // here, off the event loop.
        if let Some(warm) = &self.warm {
            let compiled = warm
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .prepare_passes(sources.plan());
            if compiled > 0 {
                log::debug!("preview: compiled {compiled} new effect pipelines ahead of drawing");
            }
        }
        Ok(sources)
    }

    /// Draws [`Monitor::frame_sources`] into a texture on the device this
    /// monitor was given: `Rgba8Unorm`, `spec.width` by `spec.height`,
    /// bindable and renderable. Errs without a device, or once the device
    /// is lost.
    ///
    /// # Call this from the thread that owns the device, and nowhere else
    ///
    /// That device is the window's, and the window's renderer took the
    /// native queue out of it and submits to that queue itself, from the
    /// event loop, outside anything wgpu locks. A queue is externally
    /// synchronised in every one of the three APIs underneath: two threads
    /// submitting to one is undefined, and what it does is not a wrong
    /// pixel. On Mesa's Intel driver it corrupts the submission the driver
    /// is building, the GPU hangs on the bad batch, and the reset takes the
    /// device down for every process on the machine - the editor, the
    /// player, the browser - until the machine is restarted (#70).
    ///
    /// So the decode goes to a worker and the drawing comes back here.
    #[cfg(feature = "gpu")]
    pub fn texture_of(
        &self,
        sources: &concat_export::PreviewSources,
        spec: FrameSpec,
    ) -> Result<wgpu::Texture, String> {
        let gpu = self
            .gpu
            .as_ref()
            .ok_or_else(|| "the monitor has no GPU device".to_owned())?;
        let mut gpu = gpu.lock().map_err(|_| "compositor poisoned".to_owned())?;
        if sources.needs_cpu() {
            // A packaged transition, and nothing else in the way, is drawn
            // whole on the device with no round trip through memory.
            if let Some(texture) = sources.transition_texture(&mut gpu) {
                return Ok(texture);
            }
            // A layer that needs FFmpeg for a package with no shader takes
            // the frame through the CPU; the picture then goes up as one
            // layer of its own.
            let frame = sources.composite(&mut *gpu);
            let mut plan = concat_render::FramePlan::empty(spec.width, spec.height);
            plan.layers.push(concat_render::PlannedLayer::picture(
                concat_render::detached_clip(),
                Arc::new(frame),
            ));
            plan.output = sources.plan().output;
            return gpu
                .render_texture(&plan)
                .ok_or_else(|| "the GPU device was lost".to_owned());
        }
        gpu.render_texture(sources.plan())
            .ok_or_else(|| "the GPU device was lost".to_owned())
    }

    /// The plan for this clip list at this size and rate: the kept one
    /// when it was built for the same list - the same allocation, or an
    /// equal one - and a fresh one otherwise, kept in its place.
    fn plan_for(
        &self,
        clips: Arc<Vec<ExportClip>>,
        settings: &DocumentSettings,
        spec: FrameSpec,
    ) -> Arc<concat_export::PreviewPlan> {
        let rate = (settings.rate_num, settings.rate_den);
        let mut slot = self
            .plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = slot.as_ref()
            && entry.width == spec.width
            && entry.height == spec.height
            && entry.rate == rate
            && entry.color_space == spec.color_space
            && (Arc::ptr_eq(&entry.clips, &clips) || *entry.clips == *clips)
        {
            return Arc::clone(&entry.plan);
        }
        let plan = Arc::new(concat_export::preview_plan(
            &clips,
            spec.width,
            spec.height,
            rate.0,
            rate.1,
            spec.color_space,
        ));
        *slot = Some(PlanEntry {
            clips,
            width: spec.width,
            height: spec.height,
            rate,
            color_space: spec.color_space,
            plan: Arc::clone(&plan),
        });
        plan
    }

    /// The engine-composited frame at one instant, as raw RGBA bytes:
    /// exactly `width * height * 4` of them.
    pub fn frame(
        &self,
        clips: Arc<Vec<ExportClip>>,
        settings: &DocumentSettings,
        spec: FrameSpec,
    ) -> Result<Vec<u8>, String> {
        let plan = self.plan_for(clips, settings, spec);
        let sources = concat_export::preview_sources_of(&self.pool, &plan, spec.time, spec.proxy)?;
        sources.pixels()
    }

    /// Decode-ahead for the playback stream: hands the scheduler the next
    /// `frames` instants after `spec.time`, so the following
    /// [`Monitor::frame`] pulls are cache hits instead of decode waits.
    /// Clamped, so a confused caller cannot queue a long decode march. The
    /// proxies are read where [`Monitor::frame`] would read them, so what
    /// is decoded ahead is what will be asked for.
    pub fn prefetch(
        &self,
        clips: Arc<Vec<ExportClip>>,
        settings: &DocumentSettings,
        spec: FrameSpec,
        frames: u32,
    ) {
        let plan = self.plan_for(clips, settings, spec);
        let moments = concat_export::preview_moments(
            &plan,
            spec.time,
            frames.min(concat_media::prefetch::AHEAD),
            spec.proxy,
        );
        let fps = (settings.rate_num as f64 / settings.rate_den.max(1) as f64).max(1.0);
        // The take a cut leads into, read early from its first frame:
        // opening a reader and seeking it costs more than the quarter second
        // the frames ahead cover, so a cut would otherwise wait on it. A
        // file the frames ahead do not read yet, or another take of one
        // they do - the pool gives it a reader of its own - and never
        // pinned: this is to have the reader open and its first frames
        // cached by the cut. Once per take: this runs every frame, the pool
        // opens a reader outside its lock, and a take sent again before its
        // first open finished would be opened again beside it.
        let mut early = self
            .early
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !spec.moving {
            early.clear();
        } else {
            // Each read of the instants ahead, with the instant it is read
            // at. `preview_moments` gives the instant a frame after the one
            // it is asked from.
            let far: Vec<(f64, concat_media::FrameRequest)> = NEXT_CUTS
                .iter()
                .flat_map(|ahead| {
                    let at = spec.time + ahead;
                    concat_export::preview_moments(&plan, at - 1.0 / fps, 1, spec.proxy)
                        .pop()
                        .map(|far| far.frames)
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |read| (at, read))
                })
                .collect();
            let near = |request: &concat_media::FrameRequest| {
                moments
                    .iter()
                    .any(|near| near.frames.iter().any(|read| same_take(read, request)))
            };
            // A take stays sent while a cut ahead or the frames ahead still
            // read it; after that, a later cut into it opens it anew.
            early.retain(|sent| near(sent) || far.iter().any(|(_, read)| same_take(sent, read)));
            let mut fresh: Vec<concat_media::FrameRequest> = Vec::new();
            for (at, request) in far {
                if !request.still
                    && !near(&request)
                    && !early
                        .iter()
                        .chain(&fresh)
                        .any(|sent| same_take(sent, &request))
                {
                    fresh.push(first_read(&plan, spec.time, at, request, fps, spec.proxy));
                }
            }
            early.extend(fresh.iter().cloned());
            if !fresh.is_empty() {
                let pool = Arc::clone(&self.pool);
                crate::scheduler().submit(concat_media::Priority::Filmstrip, move || {
                    for request in &fresh {
                        let _ = pool.frame(request);
                    }
                });
            }
        }
        drop(early);
        crate::scheduler().advance(
            concat_media::Cursor {
                time: spec.time,
                direction: concat_media::Direction::Forward,
                rate: if spec.moving { 1.0 } else { 0.0 },
            },
            1.0 / fps,
            moments,
        );
    }

    /// Forgets every cached frame, reader and plan, for when the project
    /// closes.
    pub fn clear(&self) {
        self.pool.clear();
        *self
            .plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.early
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_export::{ClipKind, TransitionSpec};

    /// Two clips of one file across a cut, the second from thirty seconds
    /// further in and dissolving in over two: the transition starts a
    /// second before the cut, at 9 s, reading the file from 29 s.
    fn across_a_cut_to_itself() -> Arc<concat_export::PreviewPlan> {
        let clip = |start: f64, source_start: f64| {
            let mut clip = ExportClip::blank(ClipKind::Video, start, 10.0, 0);
            clip.path = "/nowhere/take.webm".to_owned();
            clip.source_start = source_start;
            clip.media_width = Some(1920);
            clip.media_height = Some(1080);
            clip
        };
        let mut incoming = clip(10.0, 30.0);
        incoming.transition = Some(TransitionSpec {
            kind: "cross-fade".to_owned(),
            duration: 2.0,
        });
        Arc::new(concat_export::preview_plan(
            &[clip(0.0, 0.0), incoming],
            960,
            540,
            24,
            1,
            ColorSpace::default(),
        ))
    }

    fn reads_at(plan: &concat_export::PreviewPlan, at: f64) -> Vec<concat_media::FrameRequest> {
        concat_export::preview_moments(plan, at - 1.0 / 24.0, 1, false)
            .pop()
            .map(|moment| moment.frames)
            .unwrap_or_default()
    }

    #[test]
    fn the_two_sides_of_a_transition_are_two_takes_of_the_file() {
        let plan = across_a_cut_to_itself();
        let reads = reads_at(&plan, 9.5);
        assert_eq!(reads.len(), 2, "both sides read: {reads:?}");
        assert!(!same_take(&reads[0], &reads[1]));
        // Each side keeps its take from frame to frame. Before the cut:
        // past it the outgoing side of a file of unknown length is a held
        // frame, a take of its own.
        let later = reads_at(&plan, 9.9);
        for read in &reads {
            assert!(later.iter().any(|next| same_take(read, next)));
        }
    }

    /// A take found three seconds ahead, a second into the transition, is
    /// opened from where the transition first reads it.
    #[test]
    fn a_take_found_ahead_opens_from_its_first_read() {
        let plan = across_a_cut_to_itself();
        let found = reads_at(&plan, 10.0)
            .into_iter()
            .max_by(|a, b| a.time.as_f64().total_cmp(&b.time.as_f64()))
            .expect("the incoming side");
        let first = first_read(&plan, 7.0, 10.0, found.clone(), 24.0, false);
        assert!(same_take(&first, &found));
        assert!(
            (first.time.as_f64() - 29.0).abs() < 0.05,
            "opened at {}",
            first.time.as_f64()
        );
        // Never walked back to the playhead or before it.
        let near = first_read(&plan, 9.5, 10.0, found, 24.0, false);
        assert!(
            near.time.as_f64() > 29.5 - 0.05,
            "opened at {}",
            near.time.as_f64()
        );
    }
}
