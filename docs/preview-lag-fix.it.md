# Come è stato risolto il lag della preview sulle transizioni

Questa pagina racconta un bug della preview di Concat 0.2.7 e come è stato trovato e corretto:
i sintomi, le misure, le cause (erano tre, una dentro l'altra) e le correzioni. Si
conclude con il modo di misurare da soli e i limiti che restano.

Correzioni: commit `3064eb0` e `a92da93e` sul branch `claude/adoring-einstein-d0y2pp`,
in release dalla `v0.2.7-arch.6`.

## 1. Il sintomo

Progetto di prova: `~/Scrivania/Concat/Test`. Una sola sorgente, un episodio scaricato da
YouTube in **4K a 60 fps, AV1 dentro un `.webm`**, tagliata in quattro clip. Al secondo
29,08 della timeline c'è un taglio con la transizione **`concat.swirl`**, lunga 2,17 s,
e subito dopo un titolo.

Riproducendo da 20 s, la preview andava liscia fino a circa 28 s. Poi crollava a **un
frame ogni 1-3 secondi**, anche alla qualità più bassa (480x270).

Il log di debug (`CONCAT_LOG=debug`) separa i due costi di ogni frame della preview:

```
preview frame 480x270 at 28.171s: gathered in 1235 ms, drawn in 29 ms
```

- `drawn` è il tempo della GPU: compositing, swirl, titolo. Era di 0-30 ms.
- `gathered` è il tempo per avere in mano i frame sorgente decodificati. Era di
  **1000-3500 ms**.

Quindi la GPU non c'entrava. Il tempo si perdeva nella **lettura del file**.

## 2. Come funziona la lettura (quanto basta per capire il bug)

La preview chiede i frame al `ReaderPool` (`src/crates/concat-media/src/pool.rs`). Il pool
tiene aperti alcuni *reader*, cioè decoder FFmpeg posizionati in un punto del file, e una
cache dei frame già decodificati. Per ogni richiesta il reader sceglie tra due strade:

- **Roll**: se il frame richiesto è poco più avanti della posizione del decoder (fino a
  `ROLL_FORWARD_FRAMES` = 60 frame), decodifica in avanti fino a lì.
- **Seek**: altrimenti salta. Un seek però non atterra sul frame richiesto: atterra
  sul **keyframe precedente**, e da lì il decoder deve decodificare tutti i frame fino
  al bersaglio.

Il file di prova ha keyframe molto distanziati, come quasi tutti i video scaricati dal
web:

```sh
ffprobe -v error -select_streams v -read_intervals 24%42 \
  -show_entries packet=pts_time,flags -of csv=p=0 video.webm | grep K
# 23.633 28.183 30.567 34.567 ...
```

Ogni seek può costare **3-5 secondi di AV1 4K** da decodificare. Su una Radeon 740M,
anche con la decodifica hardware VAAPI, sono centinaia di millisecondi o qualche secondo.
Un seek a ogni frame vuol dire una preview ferma.

## 3. Le cause

### 3.1 Un salto in avanti che tornava indietro (commit `3064eb0`)

Quando la riproduzione resta indietro di più di 60 frame (a 60 fps basta un secondo), il
pool sceglieva il seek. Il seek però atterrava sul keyframe precedente, che spesso stava
**dietro** la posizione attuale del decoder. Il decoder rifaceva quindi tutto il lavoro
già fatto, più altro. Ogni frame costava secondi e la riproduzione non recuperava mai.

**Correzione.** Prima di un seek in avanti il pool chiede all'indice del container dove
atterrerebbe: `Decoder::keyframe_before()` in `decode.rs`, che usa
`avformat_index_get_entry_from_timestamp`. Se quel keyframe non è oltre la posizione del
decoder, conviene continuare a decodificare: è la funzione `rolling_is_cheaper()` in
`pool.rs`.

### 3.2 Il frame richiesto veniva scartato (commit `a92da93e`)

Matroska/WebM scrive i tempi in **millisecondi**. A 60 fps un frame dura 16,666… ms,
quindi il container lo timbra con una frazione di millisecondo di **anticipo** sul suo
tempo esatto. Il pool mandava il decoder al tempo esatto del frame, e il decoder scarta
tutto quello che è timbrato prima di quel tempo. Così scartava **proprio il frame
richiesto** e restituiva il successivo.

Risultato: il frame richiesto non finiva mai in cache. Il reader era già oltre, quindi la
richiesta successiva dello stesso frame faceva un altro seek e un altro gruppo di
immagini da decodificare.

**Correzione.** Il reader viene mandato mezzo frame prima del tempo del frame (funzione
`landing()` in `pool.rs`). Inoltre, un frame trovato oltre il bersaglio viene salvato in
cache anche sotto il numero del bersaglio.

### 3.3 Due clip, un solo decoder (commit `a92da93e`)

Era la causa principale. La transizione è a cavallo di un taglio **tra due pezzi dello
stesso file**:

| Lato della transizione | Clip | Punto del file letto |
|---|---|---|
| uscente | `c2` | 28,0 s → 30,2 s |
| entrante | `c3` | 34,1 s → 36,2 s (`sourceStart` 35,135) |

Per ogni frame della transizione la preview chiede **due** frame dello stesso file, a
**6 secondi di distanza**. Il pool identificava i reader per file, dimensione e filtri,
quindi entrambi i lati finivano sullo **stesso decoder**. A ogni frame il decoder andava
avanti di 6 s per il lato entrante (seek al keyframe 30,567, poi ~3,5 s da decodificare)
e poi tornava indietro per il lato uscente (seek al keyframe 23,633, poi ~4,5 s). Erano
due seek costosi per ogni frame mostrato, per tutta la transizione.

Il fix 3.1 non bastava, perché qui uno dei due salti è **all'indietro**, e indietro un
seek è sempre necessario.

**Correzione.** Ogni richiesta porta ora un numero di **take**
(`FrameRequest::of_take`): indica dove sta l'orologio del file rispetto a quello della
timeline. Il take fa parte della chiave del reader, quindi i due lati della transizione
hanno ciascuno il proprio decoder, e ognuno decodifica in avanti dal punto in cui si
trova. Due pezzi dello stesso take, per esempio una clip divisa in due senza spostarla,
restano sullo stesso reader, che continua attraverso il taglio.

Inoltre la preview (`src/crates/concat-host/src/preview.rs`) **apre in anticipo** il take
in cui porta un taglio: 3 s e 1 s prima (`NEXT_CUTS`), partendo dal primo frame che la
transizione leggerà. In questo modo l'apertura a freddo, con il primo seek e la
decodifica dal keyframe, è già finita quando la transizione inizia. Prima lo faceva solo
quando il taglio portava a un *altro* file.

## 4. Come è stato trovato

1. **Separare i costi.** `gathered` contro `drawn` nel log ha escluso subito la GPU.
2. **Guardare il file.** Con `ffprobe` (codec AV1, keyframe ogni 2-5 s) si è visto quanto
   costa ogni seek.
3. **Guardare il progetto.** Da `concat.json`: tutte le clip hanno lo stesso `mediaId` e
   la transizione sovrappone due punti dello stesso file. Da qui l'ipotesi del ping-pong
   su un solo decoder.
4. **Riprodurre senza interfaccia.** Un piccolo programma ha chiesto al `ReaderPool` gli
   stessi frame della transizione, alternando uscente ed entrante come fa la preview:

   | | Media per coppia di frame | Caso peggiore |
   |---|---|---|
   | un decoder per file | 2015 ms | 4965 ms |
   | un decoder per lato | 19 ms | 1079 ms (solo l'apertura del secondo) |

5. **Misurare il progetto vero, sempre senza interfaccia**, con il comando aggiunto per
   questo scopo:

   ```sh
   cd src
   cargo build --profile quick -p concat-cli
   ./target/quick/concat-cli bench-project ~/Scrivania/Concat/Test --from 20 --seconds 20 --quality quarter
   ```

   Riproduce la timeline sull'orologio reale e stampa, secondo per secondo, media, p95,
   caso peggiore e frame mai mostrati.
6. **Confermare nell'app** con il log completo: `gathered` medio, frame lenti, buchi nella
   sequenza dei tempi.

Un controllo utile lungo la strada è stato escludere i proxy. Il benchmark è stato
rilanciato con un carico equivalente alla scrittura di un proxy in parallelo (ffmpeg:
decodifica software a 2 thread + x264 `veryfast` a 2 thread). La transizione è rimasta a
3-9 ms per frame, quindi i proxy non erano la causa.

## 5. Risultati

Radeon 740M (RADV + VAAPI), progetto `Test`, riproduzione da 20 s.

| | Prima | Dopo |
|---|---|---|
| `gathered` sulla transizione (app) | 1000-3500 ms | 13-20 ms |
| `gathered` medio da 20 s a 46 s (app, 960x540) | — | **4,8 ms** |
| Frame oltre 16 ms (app) | quasi tutti | 4 su 630, più la partenza a freddo (867 ms) |
| Qualità del monitor | scendeva a 480x270 | resta a 960x540 |
| `bench-project` da 26 s per 10 s | peggiore 7303 ms, 194 frame su 240 mai mostrati | peggiore 20,7 ms dopo la partenza, 0 frame persi |
| `bench-project` da 20 s per 20 s, quarter | — | transizione 3-6 ms/frame |

## 6. Test

In `src/crates/concat-media/src/pool.rs` e `src/crates/concat-host/src/preview.rs`:

- `a_jump_ahead_inside_the_same_group_of_pictures_rolls_on` e
  `a_long_jump_inside_one_group_of_pictures_decodes_on`: un salto in avanti dentro lo stesso
  gruppo di immagini continua a decodificare invece di tornare al keyframe.
- `the_two_sides_of_a_transition_are_two_takes_of_the_file`: i due lati di una transizione
  tra pezzi dello stesso file sono due take, e ognuno resta il suo da un frame all'altro.

Da `src/`:

```sh
cargo test --profile quick -p concat-media -p concat-host -p concat
```

Se la macchina ha poca RAM, compila un crate alla volta con `-j 2`: il crate `concat`
arriva da solo a ~10 GB di RAM durante la compilazione.

## 7. Cosa resta

- **La partenza a freddo** (il primo frame dopo il play, o dopo un salto sul righello) su un
  file 4K AV1 costa ancora 0,7-1 s, perché il primo seek decodifica dal keyframe
  precedente. Il rimedio è il proxy.
- **I proxy scritti prima di `f7c8cca`** hanno un altro nome, perché nell'hash c'è la
  distanza tra keyframe. Vengono cancellati e riscritti da zero: per un file 4K lungo
  servono diversi minuti, e una scrittura interrotta alla chiusura dell'app riparte da
  capo all'apertura successiva.
- **Tre o più punti dello stesso file in un solo frame** (per esempio una transizione più
  un picture-in-picture della stessa sorgente) hanno ciascuno il proprio take, quindi
  altrettanti decoder aperti, entro il limite di reader del pool.
