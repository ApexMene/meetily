# Glossario e valori incerti

Tutto e' "fail safe": un errore lascia il testo com'era e la trascrizione continua.

## Glossario

Impostazioni in Settings > Transcription (`glossary.json` nella cartella dati).

- Whisper: i termini diventano l'`initial_prompt` ("Glossario: A, B, C."), limitato a 600 caratteri.
- Tutti i motori (Parakeet e gli altri): correzione prudente del testo gia' trascritto (`glossary/fuzzy.rs`).
  Una differenza di sole maiuscole viene sempre corretta; un errore di battitura solo per termini di almeno
  5 lettere, con la stessa iniziale e somiglianza >= soglia (default 0.85, minimo 0.7). Le parole con cifre
  non si toccano e una parola che e' gia' un altro termine del glossario non viene cambiata.
  Si applica al testo dal vivo e alla ri-trascrizione dopo il meeting.

## Valori incerti

Dopo la ri-trascrizione (`post_meeting/retranscribe.rs`) il testo preciso viene confrontato con quello dal
vivo (`transcripts.live.json`). Ogni valore con cifre (numeri, versioni, IP, date, orari) che la lettura
precisa ha e quella dal vivo non ha viene scritto con " (?)" (`glossary/critical.rs`, `second_pass.rs`).
Non si marca nulla se la lettura dal vivo non ha valori critici (puo' aver scritto il numero a parole).
I prompt di pulizia e del resoconto Notion ricevono l'istruzione di non dare mai per certo un valore con
"(?)" e di portare il segno in riassunto e task, con la nota "da verificare".

## Limite di tempo

La ri-trascrizione ha un tempo massimo (default 45 minuti, 5-480). Scaduto, viene annullata (il database si
scrive solo alla fine, quindi nulla cambia) e resta il testo dal vivo: il resoconto non viene ritardato oltre.

## Non fatto

- La seconda passata riguarda tutta la registrazione (e' la ri-trascrizione che gia' esisteva), non solo i
  segmenti critici: scegliere i segmenti richiederebbe di cambiare il motore di ri-trascrizione.
- I segmenti a bassa confidenza non sono marcati (la confidenza non e' salvata nel database).
- Numeri scritti a parole ("dieci") non sono confrontati con le cifre.

## Test

Logica pura con test unitari (`fuzzy.rs`, `critical.rs`, `terms.rs`, `second_pass.rs`), anche su input
degeneri. Non provato dal vivo: un meeting reale con Whisper/Parakeet, il budget di tempo scaduto, il resoconto Claude.
