# Recording guard

Rende la registrazione affidabile. Tutto e' "fail safe": un errore viene scritto nel log e la
registrazione e la trascrizione continuano.

## Cosa fa

- **Avviso canale muto.** Un osservatore (tick di 3 s) legge il livello RMS dei due canali. Avvisa con una
  notifica (una sola per episodio) se il canale "Others" e' muto per piu' di N secondi (default 90) mentre
  si sta parlando o mentre un'app di meeting usa il microfono (Windows), oppure se il microfono e' muto
  per 2N secondi mentre gli altri parlano. Soglia (default -70 dBFS) e tempo sono nelle impostazioni.
- **Marcatore di buco.** Quando il microfono viene sostituito dopo una disconnessione, nella trascrizione
  compare `[audio interrotto mm:ss (N s)]`.
- **Salvataggio a pezzi a prova di crash.** I pezzi da 30 s in `.checkpoints/` vengono scritti prima in
  `.part` e poi rinominati; se il disco e' pieno la registrazione continua e la memoria resta limitata
  (limite di pezzi in RAM, con attesa crescente fra i tentativi). Il merge finale non cancella mai i pezzi
  se fallisce.
- **Recupero all'avvio.** 20 s dopo l'avvio, le cartelle con pezzi ma senza `audio.mp4` vengono unite in
  `audio.mp4` (i pezzi restano) e arriva una notifica. Mai durante una registrazione.

## Non fatto

Il riaggancio automatico del canale di sistema (Windows) dopo il cambio dispositivo non e' stato
implementato: non si puo' verificare senza dispositivi reali. L'avviso di canale muto copre quel caso.

## Test

Logica pura (`recording_guard/silence.rs`, `recovery.rs`, `audio/checkpoint_files.rs`) con test unitari,
anche sui percorsi di errore. Non provati dal vivo: chiamata Teams reale, scollegamento dispositivi, crash.
