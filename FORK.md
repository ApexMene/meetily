# Fork di Meetily — procedura di manutenzione e rilascio

Questo fork (`LeoRock-13/meetily`) distribuisce una build Windows propria. L'app installata
riceve gli aggiornamenti **solo** dalle release di questo fork, firmate con la chiave del fork.

## Principio: non toccare i file upstream

Tutto ciò che è specifico del fork vive in file che upstream non ha, così i merge da upstream
non vanno in conflitto:

| File | Scopo |
|---|---|
| `frontend/src-tauri/tauri.fork.conf.json` | Livello di config unito a `tauri.conf.json` in build (`--config`): versione, chiave pubblica e endpoint dell'updater, solo installer NSIS |
| `.github/workflows/fork-release.yml` | Workflow di release solo Windows (riusa `build.yml` di upstream) |
| `FORK.md` | Questo documento |

`tauri.conf.json` resta identico a upstream. **Non** modificare lì pubkey, endpoint o versione.

### Funzionalità del fork

- **Modelli custom** per trascrizione e riassunti (Hugging Face / file locali): design, file toccati e test in
  [docs/fork/custom-models.md](docs/fork/custom-models.md).
  Codice nuovo isolato in `frontend/src-tauri/src/custom_models/` e
  `frontend/src/components/CustomModels/`; nei file upstream solo agganci piccoli
  (marcati con il commento `Fork:`), per ridurre i conflitti nei merge.
- **Provider "Claude Code"** per i riassunti: usa la CLI Claude Code dell'utente (login con
  l'account Claude, nessuna API key), nativa o in WSL. Dettagli in
  [docs/fork/claude-code-provider.md](docs/fork/claude-code-provider.md).
  Codice in `frontend/src-tauri/src/claude_code/` e `frontend/src/components/ClaudeCode/`.
- **Meeting automatici e lavoro dopo il meeting** (tutto spento di default): registrazione automatica
  quando un'app di meeting usa il microfono, barra del meeting, scorciatoia globale, cancellazione
  dell'audio vecchio, avvio con Windows, filtro "solo alfabeto latino" per Parakeet, nuova trascrizione
  a fine meeting con pulizia di Claude e resoconto in Notion. Dettagli in
  [docs/fork/meeting-automation.md](docs/fork/meeting-automation.md). Codice in
  `frontend/src-tauri/src/meeting_automation/`, `post_meeting/`, `notion_agent/` e
  `frontend/src/components/ForkSettings.tsx`.
- **Unire meeting e togliere parti di un meeting**: Ctrl+clic su più meeting nella barra laterale e
  **Merge**; nella pagina del meeting **Remove parts…** toglie i segmenti scelti dalla trascrizione e
  gli stessi intervalli dall'audio. Dettagli in [docs/fork/meeting-edit.md](docs/fork/meeting-edit.md).
  Codice in `frontend/src-tauri/src/meeting_edit/` e `frontend/src/components/MeetingEdit/`.
- **Glossario e valori incerti** (Settings > Transcription): elenco di termini da scrivere giusti
  (suggerimento a Whisper, correzione prudente dei quasi-errori per tutti i motori); dopo il meeting la
  ri-trascrizione precisa viene confrontata con quella dal vivo e i valori critici su cui le due letture
  non concordano (numeri, versioni come 1.0.0g, IP, date, orari) sono marcati "(?)" e Claude li porta nel
  resoconto e nei task. La ri-trascrizione ha un limite di tempo, scaduto il quale resta il testo dal
  vivo. Dettagli in [docs/fork/glossary-numbers.md](docs/fork/glossary-numbers.md). Codice in
  `frontend/src-tauri/src/glossary/`.

Durante un merge da upstream, cercare `Fork:` per ritrovare tutti gli agganci.

## Build locale su Windows

Prerequisiti: Visual Studio 2022 (C++ e CMake), Rust 1.99.1 (`rustup`), Node 20+, pnpm 9.15.9
(`corepack pnpm@9.15.9`). `whisper-rs` genera i binding con bindgen: serve **libclang 18**
(con LLVM 23 i binding Windows escono incompleti). Senza diritti admin basta la DLL del pacchetto
PyPI `libclang==18.1.1` e `LIBCLANG_PATH` che punta alla sua cartella `clang/native`.

```bash
cargo build -p llama-helper && cp target/debug/llama-helper.exe frontend/src-tauri/binaries/llama-helper-x86_64-pc-windows-msvc.exe
cd frontend && corepack pnpm@9.15.9 install --frozen-lockfile && corepack pnpm@9.15.9 run tauri:dev
```

## Branch

- `main` — copia esatta di upstream. Si aggiorna solo con **Sync fork** su GitHub. Nessun commit diretto.
- `custom` — branch di rilascio: modifiche del fork + merge da `main`. Il workflow parte solo da qui.

## Versioni

Formato: `<versione upstream>-<build del fork>`, solo numeri. Esempi: `0.5.0-1`, `0.5.0-2`, poi `0.5.1-1`.

- L'updater confronta in semver: `0.5.0-2` > `0.5.0-1`, `0.5.1-1` > `0.5.0-9`.
- Si pubblica solo l'installer NSIS (`-setup.exe`): l'MSI gestisce male le versioni pre-release.
- Il workflow rifiuta formati diversi e tag già esistenti, e avvisa se la base non coincide
  con la versione in `tauri.conf.json`.

## Ciclo di rilascio

1. Su GitHub: **Sync fork** di `main`.
2. In locale:
   ```bash
   git fetch origin
   git checkout custom
   git merge origin/main        # solo le novità upstream che vuoi
   ```
3. Aggiorna `version` in `frontend/src-tauri/tauri.fork.conf.json` (es. upstream passa a 0.5.1 → `0.5.1-1`).
4. `git commit` + `git push origin custom`.
5. GitHub → **Actions → Fork Release (Windows) → Run workflow** sul branch `custom`.
   La build dura indicativamente 40–60 minuti.
6. Al termine c'è una **release in bozza** con `meetily_<versione>_x64-setup.exe`, `.sig` e `latest.json`.
   Il workflow ha già verificato la firma con la chiave pubblica del fork.
7. Scarica e prova l'installer.
8. **Publish release**. Da quel momento le app installate vedono l'aggiornamento
   (`releases/latest` ignora le bozze).

Se una build fallisce, elimina la bozza (e il tag, se creato) e rilancia.

## Chiave di firma dell'updater

- Privata: `%USERPROFILE%\.tauri\meetily-fork.key` (+ password), copia nel password manager.
- Pubblica: in `tauri.fork.conf.json` → `plugins.updater.pubkey`.
- Secrets GitHub del fork (Actions): `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
  Impostali dal file, senza copia-incolla (PowerShell):
  ```powershell
  Get-Content -Raw "$env:USERPROFILE\.tauri\meetily-fork.key" | gh secret set TAURI_SIGNING_PRIVATE_KEY -R LeoRock-13/meetily
  gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R LeoRock-13/meetily
  ```
  Il workflow si ferma subito se mancano o se la chiave non è nel formato giusto.

**Se la chiave privata va persa** le app installate rifiuteranno ogni aggiornamento futuro:
serve generare una nuova coppia, aggiornare la pubkey e reinstallare a mano su ogni PC.
Non committare mai la chiave privata.

Rigenerare (solo in caso di perdita o compromissione):
```bash
npx --yes @tauri-apps/cli@2.11.1 signer generate -w "$HOME/.tauri/meetily-fork.key"
```

## Prima installazione

- La build ufficiale di upstream cerca aggiornamenti su upstream: la prima volta va installata
  a mano una build del fork (stesso identificativo `com.meetily.ai`, quindi dati e modelli restano).
- L'installer non ha firma Authenticode: Windows SmartScreen mostra un avviso
  (*Ulteriori informazioni → Esegui comunque*). Gli aggiornamenti successivi sono comunque
  verificati dalla firma minisign.
