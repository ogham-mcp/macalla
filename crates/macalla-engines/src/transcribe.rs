// SPDX-License-Identifier: MIT
//! transcribe.cpp, the runtime under every non-Apple engine.
//!
//! What the engines on it share: loading a model quietly, and for a streaming
//! model, a session that lives on a thread of its own.
//!
//! Why a thread. `transcribe_cpp::Session::stream` returns a stream that
//! borrows the session for as long as it is open, and
//! [`macalla_core::stt::SttEngine::stream`] has to return a stream that owns
//! everything it needs. A value holding both a session and a borrow of that
//! same session cannot be written without `unsafe`. So the session never
//! leaves one thread: the engine and each stream send it work over channels,
//! and the borrow begins and ends inside that thread's loop.
//!
//! Input is a bounded channel, so a file pushed faster than the model takes it
//! waits instead of piling up in memory -- the backpressure the Apple engine
//! applies too. Live audio never gets that far ahead.
//!
//! One stream at a time, which is the product: the record loop finishes or
//! drops a stream before it opens the next. A second stream opened while the
//! first is still alive waits behind it.

use crate::models::{Family, ModelSpec};
use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
use macalla_core::stt::{SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Mutex;

/// Open the engine for a model already in the store. The registry has checked
/// the language and found the file; the family decides the rest.
pub(crate) fn open(spec: &'static ModelSpec, path: &Path) -> Result<Box<dyn SttEngine>> {
    Ok(match spec.family {
        Family::Parakeet => Box::new(crate::parakeet::ParakeetEngine::load(path)?),
        Family::Nemotron => Box::new(crate::nemotron::NemotronEngine::load(path)?),
        Family::Whisper => Box::new(crate::whisper::WhisperEngine::load(spec, path)?),
    })
}

/// Pushes queued for the session before the next one waits: five seconds of
/// the record loop's 100 ms chunks.
const QUEUE: usize = 50;

/// A model and one session on it.
pub(crate) struct Loaded {
    pub model: transcribe_cpp::Model,
    pub session: transcribe_cpp::Session,
}

/// Load a model from a file in the store. The first load on a machine compiles
/// the Metal shader library and takes around ten seconds; every later one is
/// under half a second.
pub(crate) fn load(path: &Path, engine: &'static str) -> Result<Loaded> {
    // The library prints decoder statistics to stderr on every run.
    // Once, before any model exists, per its contract.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(transcribe_cpp::disable_logging);
    let started = std::time::Instant::now();
    let model = transcribe_cpp::Model::load(path).map_err(|e| Error::Stt {
        engine,
        message: format!("loading {}: {e}", path.display()),
    })?;
    let session = model.session().map_err(|e| Error::Stt {
        engine,
        message: format!("opening a session: {e}"),
    })?;
    tracing::info!(
        engine,
        backend = %model.backend(),
        load_ms = started.elapsed().as_millis() as u64,
        "model loaded"
    );
    Ok(Loaded { model, session })
}

/// A session for one-shot runs, the batch engines' path. One session is not
/// reentrant and `SttEngine::transcribe` takes `&self`, so it sits behind a
/// lock; one dictation at a time is the product anyway. Unlike [`Worker`] it
/// frees the model on the thread that drops it.
pub(crate) struct Batch {
    engine: &'static str,
    session: Mutex<transcribe_cpp::Session>,
    model: transcribe_cpp::Model,
}

impl Batch {
    pub(crate) fn new(loaded: Loaded, engine: &'static str) -> Self {
        Self {
            engine,
            session: Mutex::new(loaded.session),
            model: loaded.model,
        }
    }

    pub(crate) fn model(&self) -> &transcribe_cpp::Model {
        &self.model
    }

    /// Transcribe audio that already exists, as one committed transcript.
    pub(crate) fn run(
        &self,
        audio: &AudioSegment,
        options: &transcribe_cpp::RunOptions,
    ) -> Result<Transcript> {
        let started = std::time::Instant::now();
        let result = {
            // Recovered, not refused, if an earlier run panicked: see
            // `lock_recovering` for why that is safe for this FFI session.
            let mut session = crate::session_lock::lock_recovering(&self.session, self.engine);
            session.run(&audio.samples, options)
        }
        .map_err(|e| Error::Stt {
            engine: self.engine,
            message: e.to_string(),
        })?;
        tracing::debug!(
            engine = self.engine,
            audio_ms = audio.duration_ms(),
            ms = started.elapsed().as_millis() as u64,
            "transcribed"
        );
        Ok(Transcript {
            text: result.text.trim().to_string(),
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

enum Job {
    Stream {
        run: transcribe_cpp::RunOptions,
        input: Receiver<Input>,
        output: Sender<Output>,
    },
    Run {
        samples: Vec<f32>,
        run: transcribe_cpp::RunOptions,
        reply: Sender<std::result::Result<String, String>>,
    },
}

enum Input {
    Samples(Vec<f32>),
    Finish,
}

enum Output {
    /// The text so far, whenever it changed.
    Text(String),
    Final(String),
    Failed(String),
}

/// A session on a thread of its own.
///
/// Dropping it closes the queue and waits for the thread to free the model.
/// The wait is the point: a model still being freed on another thread while
/// the process exits runs into ggml's own teardown of the Metal device, which
/// asserts and aborts -- `macalla transcribe` did exactly that, after printing
/// a complete transcript, until this joined. A stream still open at that
/// moment would keep the thread waiting on its input, so a stream must not
/// outlive its engine; the record loop finishes or drops it long before.
pub(crate) struct Worker {
    engine: &'static str,
    /// `None` only while dropping, so the queue is closed before the join.
    jobs: Option<SyncSender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn spawn(loaded: Loaded, engine: &'static str) -> Result<Self> {
        let (jobs, queue) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name(format!("{engine} session"))
            .spawn(move || serve(loaded, queue))
            .map_err(|e| Error::Stt {
                engine,
                message: format!("starting the session thread: {e}"),
            })?;
        Ok(Self {
            engine,
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    fn send(&self, job: Job) -> Result<()> {
        self.jobs
            .as_ref()
            .ok_or_else(|| stopped(self.engine))?
            .send(job)
            .map_err(|_| stopped(self.engine))
    }

    pub(crate) fn stream(&self, run: transcribe_cpp::RunOptions) -> Result<Box<dyn SttStream>> {
        let (input, queued) = mpsc::sync_channel(QUEUE);
        let (results, output) = mpsc::channel();
        self.send(Job::Stream {
            run,
            input: queued,
            output: results,
        })?;
        Ok(Box::new(WorkerStream {
            engine: self.engine,
            input,
            output,
            pushed: 0,
        }))
    }

    /// One-shot, for audio that already exists.
    pub(crate) fn run(
        &self,
        audio: &AudioSegment,
        run: transcribe_cpp::RunOptions,
    ) -> Result<String> {
        let (reply, answer) = mpsc::channel();
        self.send(Job::Run {
            samples: audio.samples.clone(),
            run,
            reply,
        })?;
        answer
            .recv()
            .map_err(|_| stopped(self.engine))?
            .map_err(|message| Error::Stt {
                engine: self.engine,
                message,
            })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stopped(engine: &'static str) -> Error {
    Error::Stt {
        engine,
        message: "the session thread stopped".into(),
    }
}

/// The session's thread: one job at a time until the engine is dropped.
fn serve(mut loaded: Loaded, jobs: Receiver<Job>) {
    for job in jobs {
        match job {
            Job::Run {
                samples,
                run,
                reply,
            } => {
                let result = loaded
                    .session
                    .run(&samples, &run)
                    .map(|t| t.text.trim().to_string())
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            Job::Stream { run, input, output } => {
                stream(&mut loaded.session, &run, &input, &output)
            }
        }
    }
    drop(loaded.model);
}

/// One stream, start to finish, on the session's thread. Input closing without
/// a finish is an abandoned dictation, and dropping the stream resets the
/// session for the next one.
fn stream(
    session: &mut transcribe_cpp::Session,
    run: &transcribe_cpp::RunOptions,
    input: &Receiver<Input>,
    output: &Sender<Output>,
) {
    let mut stream = match session.stream(run, &transcribe_cpp::StreamOptions::default()) {
        Ok(stream) => stream,
        Err(e) => {
            let _ = output.send(Output::Failed(e.to_string()));
            return;
        }
    };
    while let Ok(item) = input.recv() {
        match item {
            Input::Samples(samples) => match stream.feed(&samples) {
                Ok(update) if update.result_changed => {
                    let _ = output.send(Output::Text(stream.text().display()));
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = output.send(Output::Failed(e.to_string()));
                    return;
                }
            },
            Input::Finish => {
                let done = match stream.finalize() {
                    Ok(_) => Output::Final(stream.snapshot().text),
                    Err(e) => Output::Failed(e.to_string()),
                };
                let _ = output.send(done);
                return;
            }
        }
    }
}

/// The record loop's end of a stream on the session's thread.
struct WorkerStream {
    engine: &'static str,
    input: SyncSender<Input>,
    output: Receiver<Output>,
    /// Samples pushed: the clock transcripts are placed on.
    pushed: u64,
}

impl WorkerStream {
    fn transcript(&self, text: &str, is_final: bool) -> Option<Transcript> {
        let text = text.trim();
        (!text.is_empty()).then(|| Transcript {
            text: text.to_string(),
            is_final,
            start_ms: 0,
            end_ms: self.pushed * 1_000 / SAMPLE_RATE as u64,
        })
    }

    fn failed(&self, message: String) -> Error {
        Error::Stt {
            engine: self.engine,
            message,
        }
    }

    /// Why the session's thread stopped listening, if it said.
    fn why_stopped(&self) -> Error {
        while let Ok(out) = self.output.try_recv() {
            if let Output::Failed(message) = out {
                return self.failed(message);
            }
        }
        stopped(self.engine)
    }
}

impl SttStream for WorkerStream {
    fn push(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.input
            .send(Input::Samples(samples.to_vec()))
            .map_err(|_| self.why_stopped())?;
        self.pushed += samples.len() as u64;
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Transcript>> {
        // Only the newest text matters: each one is the whole text so far.
        let mut latest = None;
        loop {
            match self.output.try_recv() {
                Ok(Output::Text(text)) => latest = Some(text),
                Ok(Output::Failed(message)) => return Err(self.failed(message)),
                // Sent only in answer to a finish, which consumes the stream.
                Ok(Output::Final(_)) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Err(stopped(self.engine)),
            }
        }
        Ok(latest
            .and_then(|text| self.transcript(&text, false))
            .into_iter()
            .collect())
    }

    fn finish(self: Box<Self>) -> Result<Vec<Transcript>> {
        self.input
            .send(Input::Finish)
            .map_err(|_| self.why_stopped())?;
        loop {
            match self.output.recv() {
                Ok(Output::Text(_)) => {}
                Ok(Output::Final(text)) => {
                    return Ok(self.transcript(&text, true).into_iter().collect())
                }
                Ok(Output::Failed(message)) => return Err(self.failed(message)),
                Err(_) => return Err(stopped(self.engine)),
            }
        }
    }
}
