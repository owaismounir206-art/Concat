# Concat on Arch Linux

[🇮🇹 Italiano](linux-arch.it.md)

This page covers installing Concat on Arch Linux (and Arch-based distributions such as
EndeavourOS, Manjaro, CachyOS), removing it again, and getting a smooth preview on the
integrated GPUs most laptops and mini PCs have, AMD Radeon 7xxM (RDNA3) and Intel Arc
(Lunar Lake / Xe2) among them.

## 1. GPU drivers first

On Linux, Concat draws the window, the preview and the export through **Vulkan only**. If
no Vulkan driver for your GPU is installed, the app still opens, but on *lavapipe*, a
software rasteriser, and every frame is then drawn by the CPU. The preview stutters
whatever you set. Concat says so in a notice at start.

```sh
# AMD Radeon (RADV) — e.g. Radeon 740M / 760M / 780M
sudo pacman -S --needed mesa vulkan-radeon libva

# Intel (ANV) — e.g. Arc 130V / 140V, Iris Xe, Arc A-series
sudo pacman -S --needed mesa vulkan-intel libva intel-media-driver
```

Do **not** install `amdvlk` beside `vulkan-radeon`: with two AMD Vulkan drivers the loader
may hand Concat the slower one.

Check that both work:

```sh
sudo pacman -S --needed vulkan-tools libva-utils
vulkaninfo --summary   # deviceType must be INTEGRATED_GPU (or DISCRETE_GPU), not CPU
vainfo                 # must list VAProfileH264… and VAProfileHEVC… entries
```

## 2. Install

### Arch package (recommended)

Download `Concat-<version>-linux-x86_64.pkg.tar.zst` from the Releases page, then:

```sh
sudo pacman -U ./Concat-<version>-linux-x86_64.pkg.tar.zst
```

This installs the program under `/opt/concat`, a `concat` command in `/usr/bin`, the menu
entry and the icon. The package depends on `vulkan-icd-loader` and lists the GPU drivers
above as optional dependencies. On ARM machines use the `aarch64` package (built on Arch
Linux ARM).

### AppImage

```sh
chmod +x Concat-<version>-linux-x86_64.AppImage
./Concat-<version>-linux-x86_64.AppImage
```

The AppImage needs `fuse2` (`sudo pacman -S fuse2`), or run it with
`--appimage-extract-and-run`.

### From source

```sh
sudo pacman -S --needed base-devel rustup cmake clang ffmpeg gtk3 fontconfig freetype2 \
  libxkbcommon alsa-lib vulkan-icd-loader
rustup default stable
git clone https://github.com/jub0t/Concat && cd Concat/src
cargo run --profile app -p concat
```

Arch's `ffmpeg` is version 7 or newer, which is what the engine needs. Every `cargo`
command runs in `src/`, the Cargo workspace, not in the repository root.

## 3. Uninstall

```sh
sudo pacman -Rns concat
```

That removes the program, the command, the menu entry and the icon. Your projects are your
own files and are never touched. What Concat keeps for itself stays in your home folder
until you remove it:

| Folder | What is in it |
|---|---|
| `~/.config/app.concat.editor` | Settings, recent projects, the template library |
| `~/.local/share/app.concat.editor` | Downloaded AI models, effect cards, rendered titles, logs, the GPU pipeline cache (`pipelines/`) |

To remove those as well:

```sh
rm -rf ~/.config/app.concat.editor ~/.local/share/app.concat.editor
```

(If `XDG_CONFIG_HOME` or `XDG_DATA_HOME` is set, the folders are under those instead.)
For the AppImage, delete the `.AppImage` file and the same two folders.

## 4. A smooth preview

What Concat does on its own:

- **Hardware decoding (VAAPI).** On a GPU driven by `amdgpu` (every recent Radeon) or `xe`
  (Intel Lunar Lake and newer), Concat decodes HEVC, 10-bit and other heavy video on the
  GPU's video engine. On the older Intel driver `i915` it stays in software, because VAAPI
  there has hung the whole machine (upstream issue #70). The log says which it chose:
  `VAAPI on /dev/dri/renderD128 (amdgpu)`. 8-bit H.264 stays on the CPU, which decodes it
  faster than the copy back from the chip costs.
- **Pipeline cache.** The first time an effect is shown, the GPU driver compiles it. That
  compile now happens on a background thread, and what the driver compiled is kept in
  `~/.local/share/app.concat.editor/pipelines/`. The next run, and every run after,
  shows the effect at once.
- **Adaptive quality.** When playback misses more than a tenth of its frames in a second,
  it plays one quality tier smaller (Half → Quarter) until you stop. The paused picture is
  always the quality you picked.

What you can do:

- Keep the preview at **Half** (the default) or **Quarter**. Full is for checking detail,
  not for playback.
- Keep **proxies** on in Settings: files larger than HD then play from a small copy.
- On a laptop, plug in and pick the *Performance* power profile
  (`powerprofilesctl set performance`).

### Environment variables

| Variable | Effect |
|---|---|
| `CONCAT_HWDEC=auto` | Default: VAAPI on `amdgpu` and `xe`, software elsewhere |
| `CONCAT_HWDEC=vaapi` | VAAPI on the first render node whatever its driver (on `i915` at your own risk, see above) |
| `CONCAT_HWDEC=off` | Software decoding everywhere |
| `CONCAT_LOG=debug` | Logs, for every preview frame, how long it took to gather and to draw |

For example `CONCAT_LOG=debug concat`. Logs are in
`~/.local/share/app.concat.editor/logs/` (Settings → Show the log).

## 5. Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| Notice "No GPU Vulkan driver found" | No Vulkan driver for your GPU | Install `vulkan-radeon` or `vulkan-intel` (section 1) |
| Log says `VAAPI is not available, decoding in software` | `libva` or the VA driver is missing | AMD: `mesa` and `libva`; Intel: `intel-media-driver`; check `vainfo` |
| Log says `VAAPI not chosen: no render node on a driver …` | Intel GPU on `i915` | Expected; `CONCAT_HWDEC=vaapi` forces it, at your own risk |
| A stall the first time an effect is applied, every run | The cache folder cannot be written | Check that `~/.local/share/app.concat.editor/pipelines` is writable |
| The window does not open | No usable GPU at all | Start from a terminal with `CONCAT_LOG=debug concat` and include the log in a bug report |
