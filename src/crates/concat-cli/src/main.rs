// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Command line driver for the Concat engine.
//!
//! This exists so the engine can be exercised end to end without a UI. The
//! `render` command is the vertical slice: probe, build a timeline, plan every
//! frame, decode, composite, encode. The `api` command is the Concat API's
//! first transport: JSON-RPC requests in, responses and events out, one per
//! line, so a script in any language edits and exports a project; `serve`
//! is the same API on a socket, for callers that are other processes.
//! `preview` draws one effect's card the way the window draws the
//! catalogue's: a still through the package's own shader on the GPU. `check` is what an author runs on
//! an effect package before sharing it: the same load the window does, its
//! fixtures, and its probes on the GPU, with every fault named. `bench`
//! times the monitor's work on this machine: a clip decoded at a preview
//! quality and drawn through effects on the GPU, frame by frame, with the
//! first frame's compile told apart from the frames after it.

use std::error::Error;
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use concat_api::rpc::{Call, Message};
use concat_core::time::{FrameRate, Rational};
use concat_core::timeline::{Clip, MediaRef, Timeline, Track, TrackKind};
use concat_media::{EncodeOptions, Encoder, FrameSink, FrameSource, ReaderPool};
use concat_render::{Compositor, WgpuCompositor, plan_frame};

#[derive(Parser)]
#[command(name = "concat-cli", version, about = "Concat engine command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report what is inside a media file.
    Probe {
        /// The file to inspect.
        path: PathBuf,
    },

    /// Decode, composite and re-encode a clip - the end-to-end slice.
    Render {
        /// Source video.
        input: PathBuf,
        /// Where to write the result.
        output: PathBuf,
        /// How many frames to render.
        #[arg(long, default_value_t = 90)]
        frames: u64,
        /// Fade up from black over this many frames. Zero disables the fade.
        #[arg(long, default_value_t = 15)]
        fade: u64,
    },

    /// Speak the Concat API: a JSON-RPC request per line on stdin, a
    /// response per line on stdout, with events from running jobs as they
    /// happen. A bare `{"method": ..., ...}` without the envelope works too.
    Api {
        /// One request to run instead of reading stdin.
        request: Option<String>,
    },

    /// Serve the Concat API on a socket until the process is stopped:
    /// JSON-RPC lines over TCP or a Unix socket, and gRPC in a build that
    /// has it. With no address given, JSON-RPC on 127.0.0.1:7420. Every
    /// connection presents a token first; with none given, one is minted
    /// and printed with the addresses.
    Serve {
        /// The TCP address for JSON-RPC lines, e.g. 127.0.0.1:7420.
        #[arg(long)]
        json: Option<SocketAddr>,
        /// A Unix socket path for JSON-RPC lines.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// The TCP address for gRPC. Needs a build with the `grpc` feature.
        #[arg(long)]
        grpc: Option<SocketAddr>,
        /// The token every connection presents first. Minted when not
        /// given, and printed either way.
        #[arg(long, env = "CONCAT_API_TOKEN")]
        token: Option<String>,
        /// A folder the API may write under: created projects, exports,
        /// preview files. Repeatable. Your home when none is given; `/`
        /// for anywhere.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
    },

    /// Draw one effect's card - a still through the package's own shader
    /// on the GPU, at its defaults or its manifest's `[card]` settings,
    /// the way the window draws the catalogue's - and write it as a JPEG:
    /// `concat-cli preview still.jpg out.jpg --effect concat.vignette`.
    Preview {
        /// A still, or the first frame of a video.
        input: PathBuf,
        /// Where to write the JPEG.
        output: PathBuf,
        /// The package's catalogue id or alias, e.g. `concat.vignette`.
        #[arg(long)]
        effect: String,
        /// Card width in pixels; the picture is scaled before the effect
        /// runs, so pixel-sized effects look as they will on the card.
        #[arg(long, default_value_t = concat_host::cards::WIDTH)]
        width: u32,
        /// Card height in pixels.
        #[arg(long, default_value_t = concat_host::cards::HEIGHT)]
        height: u32,
    },

    /// Time the monitor's work on this machine: `input` decoded at a
    /// preview quality and drawn through `effects` on the GPU the way the
    /// window draws a frame, then how long each part took, the frames a
    /// second that come to, and whether the first frame with the effects
    /// had to wait on compiling them:
    /// `concat-cli bench clip.mp4 --effects concat.vignette,concat.glow`.
    Bench {
        /// The clip to play.
        input: PathBuf,
        /// Effect ids, comma-separated, drawn in order at their defaults.
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "concat.vignette,concat.glow"
        )]
        effects: Vec<String>,
        /// The monitor's quality: full, half or quarter of the clip's size.
        #[arg(long, default_value = "half")]
        quality: String,
        /// How many frames to time.
        #[arg(long, default_value_t = 150)]
        frames: u64,
        /// Where to keep the GPU's compiled pipelines between runs, as the
        /// window does under its data folder; a second run then shows what
        /// the window's second start costs. None by default: every run
        /// compiles from nothing.
        #[arg(long)]
        pipeline_cache: Option<PathBuf>,
        /// The frame rate the timing is held against.
        #[arg(long, default_value_t = 30.0)]
        target_fps: f64,
    },

    /// Check effect packages before sharing them: load each folder the way
    /// the window does, hold its id against the built-ins, run its
    /// fixtures and its probes, and name every fault. `path` is one package folder - the
    /// one with `effect.toml` in it - or a folder of them, such as the
    /// app's own effects folder. Exits non-zero when any package fails.
    Check {
        /// A package folder, or a folder of package folders.
        path: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    // A crash names the module it happened in on standard error, as the
    // window's does; see concat-host's crash.rs.
    concat_host::crash::catch();
    match Cli::parse().command {
        Command::Probe { path } => probe(&path),
        Command::Render {
            input,
            output,
            frames,
            fade,
        } => render(&input, &output, frames, fade),
        Command::Api { request } => api(request),
        Command::Serve {
            json,
            socket,
            grpc,
            token,
            roots,
        } => serve(json, socket, grpc, token, roots),
        Command::Preview {
            input,
            output,
            effect,
            width,
            height,
        } => preview(&input, &output, &effect, width, height),
        Command::Check { path } => check(&path),
        Command::Bench {
            input,
            effects,
            quality,
            frames,
            pipeline_cache,
            target_fps,
        } => bench(
            &input,
            &effects,
            &quality,
            frames,
            pipeline_cache,
            target_fps,
        ),
    }
}

/// The stdin transport. Every line in is one call; every line out is one
/// JSON object, a response or an event, written whole under stdout's lock
/// and flushed, so a caller reading a pipe sees progress as it happens and
/// never half a line. A line that is not a call gets an error response and
/// the loop goes on. At the end of input the jobs still running - an export
/// begun on the last line - are waited for, so their events are written
/// before the process exits.
fn api(single: Option<String>) -> Result<(), Box<dyn Error>> {
    let mut api = concat_api::Api::new(Arc::new(|event| emit(&Message::Event(event))))?;
    let mut serve = |line: &str| {
        let (id, response) = match Call::parse(line) {
            Ok(call) => (call.id, api.dispatch(call.request)),
            Err((id, error)) => (id, concat_api::Response::Error(error)),
        };
        emit(&Message::Reply { id, response });
    };
    match single {
        Some(line) => serve(&line),
        None => {
            for line in std::io::stdin().lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                serve(&line);
            }
        }
    }
    api.finish();
    Ok(())
}

/// One line to stdout. A line that cannot be written is a caller that
/// went away, and the loop learns that from stdin's end.
fn emit(message: &Message) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", message.to_json());
    let _ = out.flush();
}

/// The socket transports, until the process is stopped. Where they listen
/// is printed, one line each, and then the token they take, so a script
/// that started this knows where to connect and what to present.
fn serve(
    json: Option<SocketAddr>,
    socket: Option<PathBuf>,
    grpc: Option<SocketAddr>,
    token: Option<String>,
    roots: Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let nothing_asked = json.is_none() && socket.is_none() && grpc.is_none();
    let config = concat_server::Config {
        json: json.or_else(|| nothing_asked.then(|| DEFAULT_JSON.parse().expect("an address"))),
        socket,
        grpc,
        token,
        roots,
    };
    let server = concat_server::Server::start(config, concat_api::Api::new)?;
    if let Some(address) = server.json_addr() {
        println!(
            "Concat API {}: JSON-RPC on {address}",
            concat_api::API_VERSION
        );
    }
    if let Some(path) = server.socket_path() {
        println!(
            "Concat API {}: JSON-RPC on {}",
            concat_api::API_VERSION,
            path.display()
        );
    }
    if let Some(address) = server.grpc_addr() {
        println!("Concat API {}: gRPC on {address}", concat_api::API_VERSION);
    }
    println!(
        "Concat API {}: token {}",
        concat_api::API_VERSION,
        server.token()
    );
    let roots = server.roots();
    println!(
        "Concat API {}: writes under {}",
        concat_api::API_VERSION,
        if roots.is_empty() {
            "anywhere".to_owned()
        } else {
            roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    loop {
        std::thread::park();
    }
}

/// Where `serve` listens when not told: loopback, on a port nothing else
/// is known to use.
const DEFAULT_JSON: &str = "127.0.0.1:7420";

/// `effect`'s card drawn from `input`, scaled to `width` by `height`, as
/// a JPEG at `output`.
fn preview(
    input: &Path,
    output: &Path,
    effect: &str,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn Error>> {
    let catalogue = concat_effects::Catalogue::builtin();
    let package = catalogue
        .get(effect)
        .ok_or_else(|| format!("no package answers to {effect}"))?;
    let card = concat_host::cards::Card::of(catalogue, package, Path::new(""))
        .ok_or_else(|| format!("{effect} draws no picture to make a card of"))?;
    let compositor =
        WgpuCompositor::new().ok_or("no GPU or software renderer is available to draw with")?;
    let mut painter = concat_host::cards::Painter::new(compositor, input, width, height)?;
    painter.draw_to(&card, output)?;
    println!("{} -> {}", package.id(), output.display());
    Ok(())
}

/// Every package at `path` through the loader and its fixtures, one line
/// per package and one per fault. The built-ins are what the ids are held
/// against: a user package cannot take a name the window already knows.
fn check(path: &Path) -> Result<(), Box<dyn Error>> {
    let folders = if path.join("effect.toml").is_file() {
        vec![path.to_path_buf()]
    } else {
        let folders = concat_effects::package_folders(path)?;
        if folders.is_empty() {
            return Err(format!(
                "{}: no package here - a package is a folder with an effect.toml in it",
                path.display()
            )
            .into());
        }
        folders
    };
    let taken = concat_effects::Catalogue::builtin();
    let mut failed = 0;
    let mut gpu = None;
    for folder in &folders {
        let mut problems = concat_effects::Package::check_folder(folder, taken);
        if problems.is_empty() {
            problems = check_probes(folder, &mut gpu);
        }
        if problems.is_empty() {
            println!("ok    {}", folder.display());
        } else {
            failed += 1;
            println!("FAIL  {}", folder.display());
            for problem in problems {
                for line in problem.lines() {
                    println!("      {line}");
                }
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} of {} package(s) failed", folders.len()).into());
    }
    println!("{} package(s) checked", folders.len());
    Ok(())
}

/// The probes of the package in `folder`, each run on the GPU, one line
/// per probe that does not hold. The compositor is made on the first
/// package that has any, and a machine that cannot make one says so as a
/// fault: a probe that did not run has not passed.
fn check_probes(folder: &Path, gpu: &mut Option<WgpuCompositor>) -> Vec<String> {
    // A folder that does not load was reported by `check_folder`.
    let Ok(package) = concat_effects::Package::from_folder(folder) else {
        return Vec::new();
    };
    if package.probes.is_empty() {
        return Vec::new();
    }
    if gpu.is_none() {
        *gpu = WgpuCompositor::new();
    }
    let Some(gpu) = gpu.as_mut() else {
        return vec![format!(
            "{}: no GPU or software renderer is available to run its probes on",
            package.id()
        )];
    };
    let mut problems = Vec::new();
    for (n, probe) in package.probes.iter().enumerate() {
        let label = probe.label(n);
        let got = match (package.probe_pass(probe), package.probe_transition(probe)) {
            (Some(pass), _) => gpu.probe(&[pass], probe.input, 8, 0.0),
            (None, Some(cut)) => {
                gpu.probe_transition(&cut, probe.input, probe.to.unwrap_or_default(), 8, 0.0)
            }
            (None, None) => continue,
        };
        match got {
            Some(got) => {
                if let Err(error) = probe.check(got) {
                    problems.push(format!("{}: {label}\n  {error}", package.id()));
                }
            }
            None => problems.push(format!("{}: {label}: the GPU did not answer", package.id())),
        }
    }
    problems
}

fn probe(path: &PathBuf) -> Result<(), Box<dyn Error>> {
    let info = concat_media::probe(path)?;

    println!("{}", info.path.display());
    match info.duration {
        Some(duration) => println!("  duration  {:.3}s ({duration})", duration.as_f64()),
        None => println!("  duration  unknown"),
    }

    match &info.video {
        Some(video) => println!(
            "  video     #{} {} {}x{} @ {} ({})",
            video.index,
            video.codec,
            video.width,
            video.height,
            video.frame_rate,
            video.frame_rate.fps()
        ),
        None => println!("  video     none"),
    }

    // Every audio stream, the default first: a recording with its tracks
    // apart lists them all, and a clip may play any one.
    if info.audio_streams.is_empty() {
        println!("  audio     none");
    }
    for audio in &info.audio_streams {
        let name = match (audio.title.is_empty(), audio.language.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!(" \"{}\"", audio.title),
            (true, false) => format!(" [{}]", audio.language),
            (false, false) => format!(" \"{}\" [{}]", audio.title, audio.language),
        };
        println!(
            "  audio     #{} {} {} Hz, {} channels{name}",
            audio.index, audio.codec, audio.sample_rate, audio.channels
        );
    }

    Ok(())
}

/// The vertical slice.
///
/// Frames come from the reader pool by (media, source time), which is what
/// makes this loop correct for *any* plan - overlapping clips, gaps, jumps -
/// not just one clip played start to finish. The shortcut this function
/// carried for its whole early life is gone; the pool it was waiting for
/// exists (`concat_media::pool`).
fn render(input: &PathBuf, output: &PathBuf, frames: u64, fade: u64) -> Result<(), Box<dyn Error>> {
    let info = concat_media::probe(input)?;
    let video = info.require_video()?;
    let (width, height) = (video.width, video.height);
    let rate = video.frame_rate;

    let timeline = single_clip_timeline(input, width, height, rate, frames);

    let pool = ReaderPool::with_defaults();
    let mut encoder = Encoder::create(output, width, height, rate, &EncodeOptions::default())?;
    // The GPU, or the software adapter where the machine has none.
    let mut compositor =
        WgpuCompositor::new().ok_or("no GPU or software renderer is available to draw with")?;

    println!("rendering {frames} frames at {width}x{height} {rate}");

    for index in 0..frames {
        let mut plan = plan_frame(&timeline, rate.time_of_frame(index as i64));
        // The plan says what is on screen; the pool fills in the pictures.
        // A gap in the timeline is a plan with no layers, and black.
        for layer in &mut plan.layers {
            let frame = pool.frame_at(
                &layer.media,
                layer.source_time,
                width,
                height,
                false,
                None,
                None,
                None,
                // Straight to the GPU compositor: deep for an HDR source.
                true,
            )?;
            layer.source = Some(frame);
            layer.opacity *= fade_in(index, fade);
        }
        let composed = compositor.render(&plan);

        encoder.write_frame(&composed)?;
    }

    encoder.finish()?;
    println!("wrote {} frames to {}", encoder.written(), output.display());
    Ok(())
}

/// The monitor's frame size at `quality` for a clip of `width` by
/// `height`: the window's tiers, rounded to even sides as it rounds them.
fn bench_size(quality: &str, (width, height): (u32, u32)) -> Result<(u32, u32), String> {
    let scale = match quality.to_ascii_lowercase().as_str() {
        "full" => 1.0,
        "half" => 0.5,
        "quarter" => 0.25,
        other => return Err(format!("quality {other:?}: full, half or quarter")),
    };
    let side = |px: u32| ((f64::from(px) * scale).round() as u32).max(2) & !1;
    Ok((side(width), side(height)))
}

/// What a list of durations comes to: the mean, the 95th percentile and
/// the worst, in milliseconds.
fn spread(times: &[std::time::Duration]) -> (f64, f64, f64) {
    if times.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut ms: Vec<f64> = times
        .iter()
        .map(|time| time.as_secs_f64() * 1000.0)
        .collect();
    ms.sort_by(f64::total_cmp);
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let p95 = ms[((ms.len() as f64 * 0.95).ceil() as usize).clamp(1, ms.len()) - 1];
    (mean, p95, ms[ms.len() - 1])
}

/// The bench: see [`Command::Bench`]. Decoding is timed apart from
/// drawing, since the window decodes ahead on workers while it draws on
/// the event loop; the frames a second it reports are the drawing's, the
/// slower of the two when decoding cannot keep ahead, and both in a row.
fn bench(
    input: &Path,
    effects: &[String],
    quality: &str,
    frames: u64,
    pipeline_cache: Option<PathBuf>,
    target_fps: f64,
) -> Result<(), Box<dyn Error>> {
    use std::time::{Duration, Instant};

    if let Some(folder) = pipeline_cache {
        concat_render::set_pipeline_cache_dir(folder);
    }
    // What the window turns on at start: the platform's decoder, VAAPI on
    // an amdgpu or xe machine.
    concat_media::set_hardware_decode(true);

    let catalogue = concat_effects::Catalogue::builtin();
    let passes = effects
        .iter()
        .filter(|id| !id.trim().is_empty())
        .map(|id| {
            let package = catalogue
                .get(id.trim())
                .ok_or_else(|| format!("no package answers to {id}"))?;
            package
                .pass(&package.params_at(concat_effects::At::Default), None)
                .ok_or_else(|| format!("{id} has no shader to draw with"))
        })
        .collect::<Result<Vec<_>, String>>()?;

    let info = concat_media::probe(input)?;
    let video = info.require_video()?;
    let rate = video.frame_rate;
    let (width, height) = bench_size(quality, (video.width, video.height))?;
    let timeline = single_clip_timeline(&input.to_path_buf(), width, height, rate, frames);
    let pool = ReaderPool::with_defaults();
    let mut compositor =
        WgpuCompositor::new().ok_or("no GPU or software renderer is available to draw with")?;
    let adapter = compositor.adapter_info();
    println!(
        "{}: {}x{} at {rate}, monitor {width}x{height} ({quality}), {} effect(s)",
        input.display(),
        video.width,
        video.height,
        passes.len()
    );
    println!(
        "GPU {} ({:?}, {:?}), decode {}, pipeline cache {}",
        adapter.name,
        adapter.device_type,
        adapter.backend,
        // The decoder this stream opens on, not the preference: 8-bit
        // H.264 stays on the CPU whatever the machine has.
        concat_media::Decoder::open(input, &concat_media::DecodeOptions::default())
            .ok()
            .and_then(|mut decoder| {
                decoder.next_frame().ok()?;
                decoder.hardware()
            })
            .map_or("software", |device| device.label()),
        if compositor.has_pipeline_cache() {
            "on"
        } else {
            "off"
        }
    );

    let mut decoding = Vec::new();
    let mut drawing = Vec::new();
    let mut first: Option<(Duration, Duration)> = None;
    for index in 0..frames {
        let mut plan = plan_frame(&timeline, rate.time_of_frame(index as i64));
        let decoded = Instant::now();
        for layer in &mut plan.layers {
            layer.source = Some(pool.frame_at(
                &layer.media,
                layer.source_time,
                width,
                height,
                false,
                None,
                None,
                None,
                true,
            )?);
            layer.effects = passes.clone();
        }
        let decoded = decoded.elapsed();
        let drawn = Instant::now();
        // Compiled first, as the monitor's worker compiles a frame's new
        // effects before the event loop draws it: the first frame's wait
        // is then the compile, and the draw after it is a draw.
        let compiling = Instant::now();
        compositor.prepare_passes(&plan);
        let compiled = compiling.elapsed();
        compositor
            .render_texture(&plan)
            .ok_or("the GPU device was lost")?;
        if !compositor.finish() {
            return Err("the GPU did not finish the frame".into());
        }
        let drawn = drawn.elapsed();
        if first.is_none() {
            first = Some((compiled, drawn - compiled));
            continue;
        }
        decoding.push(decoded);
        drawing.push(drawn);
    }

    let budget = 1000.0 / target_fps;
    let (decode_mean, decode_p95, decode_worst) = spread(&decoding);
    let (draw_mean, draw_p95, draw_worst) = spread(&drawing);
    let both: Vec<Duration> = decoding
        .iter()
        .zip(&drawing)
        .map(|(a, b)| *a + *b)
        .collect();
    let (both_mean, both_p95, _) = spread(&both);
    // Some drivers compile when a pipeline is made, others - lavapipe
    // among them - when it is first drawn with, so the stall is told as
    // what the first frame cost beyond a steady one, wherever it fell.
    if let Some((compiled, drawn)) = first {
        let compiled = compiled.as_secs_f64() * 1000.0;
        let drawn = drawn.as_secs_f64() * 1000.0;
        println!(
            "first frame with the effects: preparing {compiled:.1} ms, drawing {drawn:.1} ms; \
             {:.1} ms beyond a steady frame",
            (compiled + drawn - draw_mean).max(0.0)
        );
    }
    println!(
        "decode   mean {decode_mean:6.2} ms  p95 {decode_p95:6.2} ms  worst {decode_worst:6.2} ms"
    );
    println!("draw     mean {draw_mean:6.2} ms  p95 {draw_p95:6.2} ms  worst {draw_worst:6.2} ms");
    println!("in a row mean {both_mean:6.2} ms  p95 {both_p95:6.2} ms");
    let ahead = decode_p95.max(draw_p95);
    println!(
        "{:.1} fps with decoding ahead (the window's way), {:.1} fps in a row; \
         {target_fps} fps needs {budget:.1} ms a frame: {}",
        1000.0 / ahead.max(f64::EPSILON),
        1000.0 / both_p95.max(f64::EPSILON),
        if ahead <= budget {
            "holds"
        } else {
            "does not hold"
        }
    );
    Ok(())
}

fn single_clip_timeline(
    input: &PathBuf,
    width: u32,
    height: u32,
    rate: FrameRate,
    frames: u64,
) -> Timeline {
    let mut timeline = Timeline::new(width, height, rate);
    let track = timeline.add_track(Track::new("V1", TrackKind::Video));
    let duration = rate.time_of_frame(frames as i64);
    timeline
        .add_clip(
            track,
            Clip::new(MediaRef::new(input), Rational::ZERO, duration),
        )
        .expect("the track was just added");
    timeline
}

/// Ramps from 0.0 to 1.0 over the first `fade` frames.
fn fade_in(index: u64, fade: u64) -> f32 {
    if fade == 0 || index >= fade {
        1.0
    } else {
        index as f32 / fade as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fade_ramps_then_holds() {
        assert_eq!(fade_in(0, 10), 0.0);
        assert_eq!(fade_in(5, 10), 0.5);
        assert_eq!(fade_in(10, 10), 1.0);
        assert_eq!(fade_in(99, 10), 1.0);
    }

    #[test]
    fn the_bench_sizes_the_monitor_as_the_window_does() {
        assert_eq!(bench_size("half", (1920, 1080)), Ok((960, 540)));
        assert_eq!(bench_size("Quarter", (3840, 2160)), Ok((960, 540)));
        assert_eq!(bench_size("full", (1001, 1001)), Ok((1000, 1000)));
        assert!(bench_size("double", (1920, 1080)).is_err());
    }

    #[test]
    fn the_spread_is_the_mean_the_95th_and_the_worst() {
        let ms = std::time::Duration::from_millis;
        let times: Vec<_> = (1..=20).map(ms).collect();
        assert_eq!(spread(&times), (10.5, 19.0, 20.0));
        assert_eq!(spread(&[]), (0.0, 0.0, 0.0));
    }

    #[test]
    fn a_zero_length_fade_is_fully_opaque_immediately() {
        assert_eq!(fade_in(0, 0), 1.0);
    }

    #[test]
    fn the_timeline_covers_exactly_the_requested_frames() {
        let timeline =
            single_clip_timeline(&PathBuf::from("a.mp4"), 1920, 1080, FrameRate::NTSC_30, 90);
        assert_eq!(timeline.frame_count(), 90);
        assert_eq!(timeline.clip_count(), 1);
    }
}
