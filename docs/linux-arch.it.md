# Concat su Arch Linux

[🇬🇧 English](linux-arch.md)

Questa pagina spiega come installare Concat su Arch Linux (e sulle distribuzioni basate su
Arch, come EndeavourOS, Manjaro e CachyOS), come rimuoverlo, e come avere una preview
fluida sulle GPU integrate di portatili e mini PC, tra cui AMD Radeon 7xxM (RDNA3) e Intel
Arc (Lunar Lake / Xe2).

## 1. Prima i driver della GPU

Su Linux Concat disegna la finestra, la preview e l'export **solo tramite Vulkan**. Se non
è installato un driver Vulkan per la tua GPU, l'app si apre lo stesso, ma su *lavapipe*, un
rasterizzatore software: ogni frame viene disegnato dalla CPU e la preview va a scatti
qualunque impostazione tu scelga. Concat lo segnala con un avviso all'avvio.

```sh
# AMD Radeon (RADV), per esempio Radeon 740M / 760M / 780M
sudo pacman -S --needed mesa vulkan-radeon libva

# Intel (ANV), per esempio Arc 130V / 140V, Iris Xe, Arc serie A
sudo pacman -S --needed mesa vulkan-intel libva intel-media-driver
```

**Non** installare `amdvlk` insieme a `vulkan-radeon`: con due driver Vulkan AMD il loader
potrebbe dare a Concat quello più lento.

Controlla che funzionino entrambi:

```sh
sudo pacman -S --needed vulkan-tools libva-utils
vulkaninfo --summary   # deviceType deve essere INTEGRATED_GPU (o DISCRETE_GPU), non CPU
vainfo                 # deve elencare voci VAProfileH264… e VAProfileHEVC…
```

## 2. Installazione

### Pacchetto Arch (consigliato)

Scarica `Concat-<versione>-linux-x86_64.pkg.tar.zst` dalla pagina Releases, poi:

```sh
sudo pacman -U ./Concat-<versione>-linux-x86_64.pkg.tar.zst
```

Il pacchetto installa il programma in `/opt/concat`, il comando `concat` in `/usr/bin`, la
voce di menu e l'icona. Dipende da `vulkan-icd-loader` e indica i driver GPU qui sopra come
dipendenze opzionali. Sulle macchine ARM usa il pacchetto `aarch64` (costruito su Arch Linux
ARM).

### AppImage

```sh
chmod +x Concat-<versione>-linux-x86_64.AppImage
./Concat-<versione>-linux-x86_64.AppImage
```

L'AppImage richiede `fuse2` (`sudo pacman -S fuse2`). In alternativa avviala con
`--appimage-extract-and-run`.

### Dai sorgenti

```sh
sudo pacman -S --needed base-devel rustup cmake clang ffmpeg gtk3 fontconfig freetype2 \
  libxkbcommon alsa-lib vulkan-icd-loader
rustup default stable
git clone https://github.com/jub0t/Concat && cd Concat/src
cargo run --profile app -p concat
```

Il pacchetto `ffmpeg` di Arch è alla versione 7 o successiva, quella richiesta dal motore.
Ogni comando `cargo` va lanciato in `src/`, che è il workspace Cargo, non nella root del
repository.

## 3. Disinstallazione

```sh
sudo pacman -Rns concat
```

Questo rimuove il programma, il comando, la voce di menu e l'icona. I tuoi progetti sono
file tuoi e non vengono mai toccati. Quello che Concat salva per sé resta nella tua home
finché non lo rimuovi:

| Cartella | Contenuto |
|---|---|
| `~/.config/app.concat.editor` | Impostazioni, progetti recenti, libreria dei template |
| `~/.local/share/app.concat.editor` | Modelli AI scaricati, anteprime degli effetti, titoli renderizzati, log, cache delle pipeline GPU (`pipelines/`) |

Per rimuovere anche queste:

```sh
rm -rf ~/.config/app.concat.editor ~/.local/share/app.concat.editor
```

Se hai impostato `XDG_CONFIG_HOME` o `XDG_DATA_HOME`, le cartelle si trovano lì dentro.
Per l'AppImage basta cancellare il file `.AppImage` e le stesse due cartelle.

## 4. Una preview fluida

Cosa fa Concat da solo:

- **Decodifica hardware (VAAPI).** Su una GPU gestita da `amdgpu` (tutte le Radeon recenti)
  o da `xe` (Intel Lunar Lake e successive), Concat decodifica HEVC, video a 10 bit e altri
  formati pesanti sul motore video della GPU. Con il vecchio driver Intel `i915` resta in
  software, perché lì VAAPI ha bloccato l'intera macchina (issue #70 del progetto). Il log
  indica la scelta: `VAAPI on /dev/dri/renderD128 (amdgpu)`. L'H.264 a 8 bit resta sulla
  CPU, che lo decodifica più in fretta di quanto costi la copia dalla GPU.
- **Cache delle pipeline.** La prima volta che un effetto viene mostrato, il driver della GPU
  lo compila. Ora la compilazione avviene su un thread in background, e il risultato viene
  salvato in `~/.local/share/app.concat.editor/pipelines/`. Dall'avvio successivo l'effetto
  compare subito.
- **Qualità adattiva.** Se in un secondo la riproduzione perde più di un decimo dei frame,
  Concat scende di un livello di qualità (Half → Quarter) fino allo stop. L'immagine in
  pausa ha sempre la qualità che hai scelto.

Cosa puoi fare tu:

- Tieni la preview a **Half** (il default) o **Quarter**. Full serve a controllare i
  dettagli, non a riprodurre.
- Tieni attivi i **proxy** nelle Impostazioni: i file più grandi dell'HD vengono riprodotti
  da una copia ridotta.
- Su un portatile collega l'alimentatore e scegli il profilo energetico *Performance*
  (`powerprofilesctl set performance`).

### Variabili d'ambiente

| Variabile | Effetto |
|---|---|
| `CONCAT_HWDEC=auto` | Default: VAAPI su `amdgpu` e `xe`, software altrove |
| `CONCAT_HWDEC=vaapi` | VAAPI sul primo render node qualunque sia il driver (su `i915` a tuo rischio, vedi sopra) |
| `CONCAT_HWDEC=off` | Decodifica software ovunque |
| `CONCAT_LOG=debug` | Registra nel log, per ogni frame della preview, quanto tempo ha richiesto prepararlo e disegnarlo |

Per esempio `CONCAT_LOG=debug concat`. I log sono in
`~/.local/share/app.concat.editor/logs/` (Impostazioni → Mostra il registro).

## 5. Risoluzione dei problemi

| Sintomo | Causa probabile | Cosa fare |
|---|---|---|
| Avviso "Driver Vulkan della GPU non trovato" | Manca il driver Vulkan della GPU | Installa `vulkan-radeon` o `vulkan-intel` (sezione 1) |
| Il log dice `VAAPI is not available, decoding in software` | Manca `libva` o il driver VA | AMD: `mesa` e `libva`; Intel: `intel-media-driver`; controlla con `vainfo` |
| Il log dice `VAAPI not chosen: no render node on a driver …` | GPU Intel con driver `i915` | È il comportamento previsto; `CONCAT_HWDEC=vaapi` lo forza, a tuo rischio |
| Uno scatto la prima volta che applichi un effetto, a ogni avvio | La cartella della cache non è scrivibile | Controlla che `~/.local/share/app.concat.editor/pipelines` sia scrivibile |
| La finestra non si apre | Nessuna GPU utilizzabile | Avvia da terminale con `CONCAT_LOG=debug concat` e allega il log a una segnalazione |
