# Macalla

![The Macalla app icon](apps/desktop/src-tauri/icons/128x128.png)

Hold a key, say what you want to write, and let go. Your words appear wherever
you were typing: an email, a document, a chat, a terminal.

Macalla runs on your Mac. Your words stay on it unless you turn on something
that sends them, and audio is never saved. There is no account to make and
nothing to pay; the cloud services you can choose to add use your own key with
that provider. It is open source, under the MIT licence.

*Macalla* is Irish for echo, literally "son of a cliff". Say it "mah-KAL-uh",
with the stress in the middle.

## Install

This first release is for Macs with Apple silicon, the M1 chip or newer, on
macOS 26 or 27, the two versions it has been tested on. It does not open on
earlier versions of macOS. Not sure what your Mac has? Open the Apple menu and
choose **About This Mac**: it names the chip and the macOS version.
Windows and Linux are on their way.

1. **Download** `Macalla_0.1.0_aarch64.dmg`, about 8 MB, from the
   [releases page](https://github.com/ogham-mcp/macalla/releases) or from
   [macalla.dev](https://macalla.dev).
2. **Open the file.** A window shows Macalla next to your Applications folder.
   Drag Macalla onto Applications.
3. **Open Macalla** from Applications. The first time, your Mac checks that it
   came from a known developer, and may ask whether you want to open an app
   from the internet. Choose **Open**. Macalla is signed and notarised by Apple.

That is the whole install. Macalla does not update itself: a new version is a
new download, installed the same way.

If you use [Homebrew](https://brew.sh), this does the same in one line, and
`brew upgrade` fetches a new version when there is one:

```sh
brew install --cask ogham-mcp/tap/macalla
```

### Three permissions

When Macalla first opens, a checklist walks you through three macOS
permissions. Each is a setting only you can turn on, and each has one job:

- **Microphone**, to hear you.
- **Input Monitoring**, to notice when you hold the shortcut.
- **Accessibility**, to paste into other apps.

macOS asks for some of them and opens System Settings for others; the
checklist says which, and ticks each one as it lands. If you turned one down,
macOS will not ask again on its own: turn it on in System Settings, in the pane
the checklist names. If holding the shortcut does nothing once all three are
ticked, quit Macalla from its menu bar icon and open it again.

### Your first dictation

1. Put the cursor where you want the text.
2. Hold **Right Option** (the default; change it on the Shortcut card) and speak.
3. Let go. The text is pasted where the cursor is.

To throw a dictation away before it is pasted, click the **×** on the overlay
at the bottom of the screen. Nothing is pasted and nothing is kept.

Afterwards Macalla waits in the menu bar. Its icon starts and stops a
dictation, picks a microphone, and opens settings and History. To have it
start when you log in, turn on **Open Macalla when you log in** on the
**General** card, at the bottom of the settings.

[How to use Macalla](docs/using-macalla.md) covers the rest: the dictionary,
Enhancement, History, and transcribing files and meetings. If something looks
wrong, [Known behaviours](docs/known-behaviours.md) lists what you may notice,
why it happens and what to do.

## Why another dictation app

There are good open-source dictation apps already. Macalla exists because of
one measured problem:

**Speech engines get your vocabulary wrong, and nothing else fixes it.**

Reading a single paragraph of ordinary technical dictation aloud, on-device:

| | Technical terms correct |
|---|---|
| Apple SpeechTranscriber | 6 of 15 |
| Parakeet TDT v3 | 9 of 15 |

Neither produced `kubectl`, `pgvector`, `Supabase` or `Ogham`. Apple gave
`postgrass` and `Cube CTL`; Parakeet gave `Superbase` and `LXE`. These are not
exotic words — they are what a working day sounds like.

Everything else the engines already do well. They punctuate, they capitalise,
they break on sentences. So Macalla does not rely on a language model to clean
up after them. It fixes the words they get wrong.

### Your words, not ours

The vocabulary Macalla ships with is an **example**. It happens to be
infrastructure terms because that is what the author dictates. Yours will be
different, and adding them is the point — drug names, case citations, client
names, product names, the German word your English engine keeps anglicising.

When a word comes out wrong, select it in History and choose **Add selection
to dictionary**: the entry opens with what you selected as the misheard form,
and you type how it should be written. The list grows from your own
corrections. Export it as JSON to share with a team or carry between machines;
import takes that JSON, a plain word list one term per line, or `heard,written`
pairs as CSV or TSV, and never removes what you already have.

And because the correction lives above the engine rather than inside it, **the
vocabulary you build survives changing engine, changing platform, or a better
model shipping next year.** That matters more than it sounds: the two engines
tested fail in *opposite* directions — one biased toward technical words, one
toward everyday ones — and each mishears what the other gets right. Read
"Clear the cache before you count the cash" aloud, and Apple Speech wrote
"cash" both times while Parakeet wrote "cache" both times.

### The rest

- **Rules first, not a model round-trip.** After the dictionary, Macalla's own
  tidying removes fillers such as "um", collapses stutters and turns spoken
  punctuation into marks. These are rules: sub-millisecond and predictable.
  Nothing on the default path waits on a language model.
- **Enhancement only if you choose it.** Enhancement is a language model
  tidying punctuation and capitals, and it is off by default. In the app you
  can pick Apple Intelligence, on a Mac that has it, your own OpenAI-compatible
  server, such as Ollama, or cloud models from providers whose key you have
  saved, ticked in the order to try them. Apple Intelligence, or a server
  running on this Mac, tidies each segment while you are still speaking. A
  server on another computer, or a cloud model, is sent the text at most once,
  when it is ready: at key-release, or when cloud transcription answers.
  Whatever a model sends back, the dictionary has the last word.
- **Enhancement waits up to 5 seconds, and says so.** With Enhancement on, the
  paste waits up to 5 seconds from the final transcript (key-release, or the
  cloud service's answer) while the overlay says "Enhancing…". Miss it and the
  rule-tidied text is pasted anyway, and a late answer goes to History. With
  Enhancement off, the text is pasted at once. That is a design rule, not an
  error path.

## Status

| | |
|---|---|
| Dictation on macOS: global shortcut, paste, History | done |
| Engines: Apple Speech on macOS; Parakeet, Whisper and Nemotron models, fetched on demand, on every platform | done |
| Dictionary and rule-based tidying | done |
| Cloud transcription, opt-in, with your own key | done |
| Enhancement: Apple Intelligence or your own server | done |
| Transcribing a file from the CLI | done |
| Enhancement with cloud models, with your own keys | done |
| Summarising a transcript file from the CLI | done |
| Files page in the app: transcribe a recording or open a transcript, tidy it, summarise it | done |
| Recording a live meeting in the app | done |
| A signed, notarised download for Apple silicon Macs | done |
| Global shortcut on Linux and Windows | not yet |
| A download for Windows and Linux | not yet |

Measured findings so far are in [`docs/benchmarks.md`](docs/benchmarks.md),
including one where the first measurement was wrong and why. Numbers there
always state the machine they were taken on.

### Measured so far

Both engines, on the prepared scripts in `fixtures/`, with Parakeet as the
app runs it; the word error rates compare them on identical audio. The figures
are from the section "Granite Speech 4.1 2B, against Parakeet and Apple" in
`docs/benchmarks.md`. Like every figure there that names no other machine, they
were taken on its reference machine, an Apple M1 Pro with 16 GB on macOS
26.6.2.

| | Apple SpeechTranscriber | Parakeet TDT v3 |
|---|---|---|
| Technical terms correct | 6 of 15 | 9 of 15 |
| Clinical terms correct | 7 of 15 | 10 of 15 |
| Word error rate, technical | 0.200 | 0.133 |
| Word error rate, clinical | 0.281 | 0.234 |
| Word error rate, everyday sentences (Harvard List 1) | 0.025 | 0.000 |

Enhancement of 20 fixtures with the seed dictionary, scored against a
reference for punctuation and capitals. The cloud rows are the four cloud
models that got 18 or more of 20 in a run. Each was sent the fixtures twice,
and the times were taken from the reference machine, an Apple M1 Pro with 16 GB
on macOS 26.6.2, including the network. The local rows come from other runs of
the same fixtures: Ollama on a Mac mini M1 with 16 GB, and Apple Intelligence on
the reference machine. The full tables are in `docs/benchmarks.md`.

| Model | Runs on | All right of 20 | p50 |
|---|---|---|---|
| gpt-5.6-luna | OpenAI | 19, 19 | 934, 841 ms |
| claude-haiku-4-5 | Anthropic | 18, 18 | 780, 808 ms |
| gemini-3.5-flash-lite | Google | 18, 17 | 628, 620 ms |
| gpt-4.1-nano | OpenAI | 18, 16 | 642, 624 ms |
| granite4.1 3B | Ollama, Mac mini M1, 16 GB | 17, 17 | 696, 710 ms |
| qwen2.5 0.5B | Ollama, Mac mini M1, 16 GB | 13, 13 | 166, 158 ms |
| Apple Intelligence, plain text | the reference machine | 11, 11 | 596, 574 ms |

Nothing has been measured on x86, on Linux or on Windows.

### Known issues

Some things you may notice come from your hardware or the operating system
rather than from Macalla, such as Bluetooth headphones sounding thinner while
their microphone records. They are in
[`docs/known-behaviours.md`](docs/known-behaviours.md), with what causes each
and what to do.

Not built yet:

- No download for Windows or Linux, and no global shortcut there: build from
  source.

## Design

- On-device by default. Anything that sends your words off the machine, cloud
  transcription or Enhancement on a server elsewhere, is opt-in, off by default,
  and named on the dictation overlay while it runs.
- No telemetry. Not even anonymous.
- The core is callable without the hotkey or the UI — the CLI drives the same
  state machine, so it works headless.
- The app is complete with the default engine for its platform. Everything
  else is a plugin.

## Building

Requires a stable Rust toolchain. macOS work additionally needs the Command
Line Tools.

```sh
cargo test --workspace
cargo run -p macalla-cli -- doctor
```

`doctor` reports the engine and permissions for the machine it runs on.

`macalla transcribe <file>` reads WAV and AIFF itself, on every platform, with
no other tool. For mp3, m4a, mp4, mov, opus, webm and the rest it uses `ffmpeg`
if it is on your PATH, streaming the audio in at 16 kHz mono. That is true on
macOS as well: ffmpeg is tried first. Only on macOS, when ffmpeg is missing or
cannot read a file, does Apple's own decoder take over for the common formats.
On Linux and Windows a compressed file needs ffmpeg. Nothing is bundled.

`macalla summarise <file>`, also spelled `summarize`, summarises a meeting
transcript: key points, decisions and action items, every one quoting the
transcript. It takes a WebVTT file, which keeps its speakers, or plain text,
and prints where the transcript goes before anything is sent.

## Speech models

The app offers these engines:

| Engine | Where it runs |
|---|---|
| Apple Speech | built into macOS, the default on a Mac |
| Parakeet TDT 0.6B v3 | downloaded model, 25 languages |
| Nemotron 3.5 ASR Streaming 0.6B | downloaded model, shows words while you speak |
| Whisper small.en and medium.en | downloaded models, English |
| Whisper medium | downloaded model, many languages |

Whisper large-v3-turbo, and Moonshine, a smaller model for older or low-end
hardware, may come later.

Voxtral is not offered on the Mac yet. It was one of the most accurate engines
on technical terms, but it needs about 4.2 GB of memory at its peak, so the
practical way to use it is through Mistral's API: choose Mistral as your cloud
transcription service, or OpenRouter, which offers Voxtral transcription models
too, with your own key.

Apple Speech needs nothing downloaded. Every other engine runs on a model file
that Macalla fetches on demand, checks against its published SHA-256, and keeps
in one folder shared by the app and the CLI, so nothing is fetched twice:

| | |
|---|---|
| macOS | `~/Library/Application Support/Macalla/models` |
| Linux | `~/.local/share/Macalla/models` |
| Windows | `%APPDATA%\Macalla\models` |

Set `MACALLA_MODELS_DIR` to keep them somewhere else, and `MACALLA_MODELS_MIRROR`
to fetch from a mirror of Hugging Face (`macalla models download --mirror`
does the same for one download). The Models page in the app shows
the folder and removes a model with one click; from a terminal,
`macalla models list`, `download`, `remove` and `import` do the same, and
`import` installs from a file already on the machine for an offline install.
A download that stops partway resumes next time. Deleting the folder by hand
is safe: the app offers the download again.

## Licence

Macalla is MIT. Model weights carry their own licences, recorded with their
obligations in [`MODELS.md`](MODELS.md). Models and model weights Macalla uses:

| Model | Licence |
|---|---|
| Apple SpeechTranscriber | Apple system component |
| Parakeet TDT 0.6B v3 | CC-BY-4.0, attribution required |
| Nemotron 3.5 ASR streaming 0.6B | OpenMDW-1.1 |
| Whisper small.en, medium.en and medium | Apache-2.0, as the Hugging Face cards for these files and for OpenAI's originals declare (OpenAI's GitHub repository is MIT) |
| earshot 1.2.2, voice activity detection | MIT OR Apache-2.0 |

---

Ogham remembers. Macalla listens.
[ogham-mcp.dev](https://ogham-mcp.dev)
