// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The driver's compiled pipelines, kept on disk between runs.
//!
//! A pass is compiled the first time its key is seen (`shader` in the
//! compositor), and on Vulkan that compile is the driver's: RADV or ANV
//! turning SPIR-V into the GPU's own code, tens to hundreds of
//! milliseconds a package on an integrated chip. It used to happen on the
//! frame that first showed the effect, which is the stall a person sees the
//! moment they drop an effect on a clip. A `wgpu::PipelineCache` hands the
//! driver what it compiled last time, so a package compiled once on this
//! machine compiles again in a fraction of that.
//!
//! The cache is a file per adapter under the folder the app names with
//! [`set_pipeline_cache_dir`], named by `wgpu::util::pipeline_cache_key`:
//! the vendor and the device, so a second GPU keeps its own. The driver
//! checks the header against itself, and a file left by another driver
//! version, or one cut short, is ignored and replaced (`fallback`). Only
//! Vulkan has the feature; elsewhere, or with no folder named, there is no
//! cache and every compile is as it was.
//!
//! The file is written after new pipelines were compiled, at most once
//! every [`SAVE_EVERY`], and when the last compositor holding the cache
//! goes: into a temporary name first and then renamed over the old one, so
//! a run that dies mid-write leaves the previous file whole.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often, at most, a cache that has new pipelines in it is written.
pub(crate) const SAVE_EVERY: Duration = Duration::from_secs(5);

/// The folder caches go into; unset means no cache on disk.
static FOLDER: OnceLock<PathBuf> = OnceLock::new();

/// Names the folder pipeline caches are kept in, for every compositor made
/// from now on. Once per process; a second call is ignored. Nothing calls
/// it in the tests, so a test never reads a cache a run left behind.
pub fn set_pipeline_cache_dir(folder: PathBuf) {
    let _ = FOLDER.set(folder);
}

/// One device's pipeline cache and the file it is kept in.
pub(crate) struct DiskCache {
    cache: wgpu::PipelineCache,
    path: PathBuf,
    /// Pipelines were compiled into the cache since it was last written.
    dirty: AtomicBool,
    /// When it was last written, or opened.
    saved: Mutex<Instant>,
}

impl DiskCache {
    /// The cache for `device`, read from its file where there is one, or
    /// `None` where the device has no pipeline cache or no folder is set.
    pub(crate) fn open(device: &wgpu::Device) -> Option<Self> {
        let folder = FOLDER.get()?;
        Self::open_in(device, folder)
    }

    /// [`DiskCache::open`] in `folder`, whatever is set.
    pub(crate) fn open_in(device: &wgpu::Device, folder: &Path) -> Option<Self> {
        if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return None;
        }
        let key = wgpu::util::pipeline_cache_key(&device.adapter_info())?;
        let path = folder.join(format!("{key}.bin"));
        let data = std::fs::read(&path).ok();
        // SAFETY: the data is only ever what `get_data` handed `save`
        // below, under this adapter's key. wgpu and the driver check its
        // header against the device before using any of it, and with
        // `fallback` a file they do not recognise - another driver, a
        // write cut short - gives an empty cache rather than an error.
        let cache = unsafe {
            device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                label: Some("concat pipelines"),
                data: data.as_deref(),
                fallback: true,
            })
        };
        log::info!(
            "pipeline cache {} ({})",
            path.display(),
            match &data {
                Some(data) => format!("{} KiB from the last run", data.len() / 1024),
                None => "new".to_owned(),
            }
        );
        Some(Self {
            cache,
            path,
            dirty: AtomicBool::new(false),
            saved: Mutex::new(Instant::now()),
        })
    }

    /// What a pipeline descriptor's `cache` takes.
    pub(crate) fn cache(&self) -> &wgpu::PipelineCache {
        &self.cache
    }

    /// Says pipelines were compiled into the cache, and writes it when the
    /// last write was long enough ago.
    pub(crate) fn compiled(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        let due = self
            .saved
            .lock()
            .map(|saved| saved.elapsed() >= SAVE_EVERY)
            .unwrap_or(false);
        if due {
            self.save();
        }
    }

    /// Writes the cache to its file, when it has anything new.
    pub(crate) fn save(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Ok(mut saved) = self.saved.lock() {
            *saved = Instant::now();
        }
        let Some(data) = self.cache.get_data() else {
            return;
        };
        if let Err(error) = write_whole(&self.path, &data) {
            log::warn!(
                "pipeline cache {} not written: {error}",
                self.path.display()
            );
        }
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        self.save();
    }
}

/// Writes `data` to `path` through a temporary file beside it, so the old
/// file is replaced whole or not at all.
fn write_whole(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, data)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_nothing_beside_it() {
        let folder = std::env::temp_dir().join(format!("concat-pipelines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let path = folder.join("nested").join("cache.bin");
        write_whole(&path, b"first").expect("writes");
        write_whole(&path, b"second").expect("writes again");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let beside: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(beside, vec![std::ffi::OsString::from("cache.bin")]);
    }

    /// On a device with a pipeline cache, the compositor's own pipelines
    /// go into it, it is written under the adapter's key, and a second
    /// open reads it back. A device without one - Metal, Direct3D - has
    /// no cache, and that is the whole of what is checked there.
    #[test]
    fn the_cache_is_written_and_read_back() {
        let Some(compositor) = crate::WgpuCompositor::new() else {
            return;
        };
        let device = &compositor.device;
        let folder = std::env::temp_dir().join(format!("concat-pipeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let Some(disk) = DiskCache::open_in(device, &folder) else {
            assert!(!device.features().contains(wgpu::Features::PIPELINE_CACHE));
            return;
        };
        let rebuilt = crate::WgpuCompositor::drawing_with(
            device.clone(),
            compositor.queue.clone(),
            compositor.work,
            Some(std::sync::Arc::new(disk)),
        );
        let path = rebuilt
            .pipeline_cache
            .as_ref()
            .map(|disk| disk.path.clone())
            .expect("the cache it was given");
        rebuilt
            .pipeline_cache
            .as_ref()
            .expect("the cache it was given")
            .save();
        drop(rebuilt);
        let written = std::fs::read(&path).unwrap_or_default();
        let again = DiskCache::open_in(device, &folder);
        let _ = std::fs::remove_dir_all(&folder);
        assert!(!written.is_empty(), "the cache was written to {path:?}");
        assert!(again.is_some(), "and opens again");
    }
}
