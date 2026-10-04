// SPDX-License-Identifier: MIT
//! What each dictation cost, and what the distribution looks like.
//!
//! The design sets a hard budget of 300 ms from key-release to paste, and the
//! breakdown is "how we prove the speed claim". A single
//! reading proves nothing: real dictations on the reference machine have come
//! back at 78, 119, 127, 153 and 267 ms, and which of those is typical is not
//! answerable from five numbers printed one at a time.
//!
//! So this accumulates them and reports percentiles. Nothing is published from
//! one sample.
//!
//! **Stages that did not run report `None`, never zero.** A zero is a
//! measurement; absent is not, and averaging an absent stage into a total is
//! how a pipeline appears to get faster by having fewer stages.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The paste budget: key-release to paste.
pub const PASTE_BUDGET: Duration = Duration::from_millis(300);

/// One dictation, timed.
///
/// The stage names follow the pipeline's stages. A stage that did not
/// run is `None` -- `vad` without the gate, `cloud` without a request,
/// `tier1` when the paste did not wait on cleanup -- and saying so is the
/// point, because a breakdown that silently omits a stage reads as a faster
/// pipeline rather than a smaller one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Dictation {
    /// Audio actually captured.
    pub audio_ms: u64,
    /// Starting the capture device, and ONLY that.
    ///
    /// It used to include the engine session below, which did not matter while
    /// a cold Bluetooth device took 3048 ms and dwarfed everything. Once the
    /// stream is built ahead of the keypress this drops to tens of
    /// milliseconds, the engine session becomes the larger half, and a single
    /// figure labelled "device open" stops being true. Measured separately so
    /// the number the prewarm is judged by is the number it changes.
    pub open_ms: Option<f64>,
    /// Opening the engine's streaming session. Paid on the keypress, before a
    /// word is heard, and on Apple SpeechTranscriber it is the larger part of
    /// the wait now that the device is armed in advance.
    pub engine_ms: Option<f64>,
    /// Voice activity detection. `None` when the gate did not run.
    pub vad_ms: Option<f64>,
    /// Key-release to the engine's final transcript. Usually the bulk. On a
    /// cloud dictation this is the on-device preview engine's; the request is
    /// `cloud_ms`.
    pub stt_ms: Option<f64>,
    /// Waiting on a cloud engine's one request for the whole recording, sent
    /// at key release: until its answer, the session's cap, or the
    /// person giving up. `None` when no request was sent -- no cloud engine,
    /// or nothing heard. Defaulted so a timing History stored before this
    /// field existed still reads, and left out when `None` so a local
    /// dictation stores the same bytes it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_ms: Option<f64>,
    /// Dictionary correction plus Tier 0.
    pub tier0_ms: Option<f64>,
    /// Tier 1 cleanup: how long the paste waited for it at release, which is
    /// never longer than the paste budget. Segments cleaned while the person
    /// was speaking cost the wait nothing. `None` when no cleanup request was
    /// sent -- cleanup off, or nothing said.
    pub tier1_ms: Option<f64>,
    /// Clipboard write, chord, wait and restore.
    pub paste_ms: Option<f64>,
    /// Key-release to text in front of the user. The number the paste budget is measured against.
    pub total_ms: f64,
    /// Samples lost because the pipeline thread fell behind. Non-zero is a bug.
    pub overruns: u64,
}

impl Dictation {
    /// Whether this one met the paste budget.
    pub fn within_budget(&self) -> bool {
        self.total_ms <= PASTE_BUDGET.as_millis() as f64
    }
}

/// Reads one stage out of a dictation. Named because the tuple array below is
/// otherwise unreadable, and because every stage is optional for the same
/// reason: a stage that did not run has no duration.
type StageOf = fn(&Dictation) -> Option<f64>;

/// Dictations so far, and what they add up to.
#[derive(Debug, Default)]
pub struct Latencies {
    samples: Vec<Dictation>,
}

impl Latencies {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, d: Dictation) {
        self.samples.push(d);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn samples(&self) -> &[Dictation] {
        &self.samples
    }

    /// Nearest-rank percentile over whichever field is asked for, skipping the
    /// dictations where that stage did not run.
    ///
    /// `None` when nothing has that stage, rather than 0.0 -- see the note at
    /// the top of the file.
    pub fn percentile(&self, p: f64, field: impl Fn(&Dictation) -> Option<f64>) -> Option<f64> {
        let mut values: Vec<f64> = self.samples.iter().filter_map(&field).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Nearest-rank, not interpolated. With the handful of samples a person
        // produces by hand, an interpolated p95 invents a number nobody
        // measured; nearest-rank always names a reading that actually happened.
        let rank = ((p / 100.0) * values.len() as f64).ceil().max(1.0) as usize;
        values.get(rank.min(values.len()) - 1).copied()
    }

    pub fn p50(&self, field: impl Fn(&Dictation) -> Option<f64>) -> Option<f64> {
        self.percentile(50.0, field)
    }

    pub fn p95(&self, field: impl Fn(&Dictation) -> Option<f64>) -> Option<f64> {
        self.percentile(95.0, field)
    }

    /// How many met the budget.
    pub fn within_budget(&self) -> usize {
        self.samples.iter().filter(|d| d.within_budget()).count()
    }

    /// A breakdown fit to print, or to paste into docs/benchmarks.md.
    ///
    /// Deliberately reports the sample count first. A p95 over five dictations
    /// is not a p95, and a table that hides `n` invites it being quoted as one.
    pub fn report(&self) -> String {
        if self.samples.is_empty() {
            return "no dictations measured".to_string();
        }
        let n = self.samples.len();
        let mut out = format!("{n} dictation{}", if n == 1 { "" } else { "s" });
        if n < 20 {
            // Say it out loud rather than letting a reader assume otherwise.
            out.push_str(" (too few for a meaningful p95)");
        }
        out.push('\n');

        let stages: [(&str, StageOf); 8] = [
            ("device start", |d| d.open_ms),
            ("engine session", |d| d.engine_ms),
            ("vad", |d| d.vad_ms),
            ("release->final", |d| d.stt_ms),
            ("cloud request", |d| d.cloud_ms),
            ("dictionary+tier0", |d| d.tier0_ms),
            ("tier1", |d| d.tier1_ms),
            ("paste", |d| d.paste_ms),
        ];
        // The cloud request is not a stage of a local session's pipeline at
        // all, so a session that never sent one gets the table it always had.
        let any_cloud = self.samples.iter().any(|d| d.cloud_ms.is_some());
        for (name, field) in stages {
            if name == "cloud request" && !any_cloud {
                continue;
            }
            match (self.p50(field), self.p95(field)) {
                (Some(p50), Some(p95)) => out.push_str(&format!(
                    "  {name:<18} p50 {p50:>7.1} ms   p95 {p95:>7.1} ms\n"
                )),
                // Named anyway. A stage missing from the table reads as a
                // faster pipeline; a stage marked "not in the path" reads as
                // what it is.
                _ => out.push_str(&format!("  {name:<18} not in the path\n")),
            }
        }

        let p50 = self.p50(|d| Some(d.total_ms)).unwrap_or_default();
        let p95 = self.p95(|d| Some(d.total_ms)).unwrap_or_default();
        out.push_str(&format!(
            "  {:<18} p50 {p50:>7.1} ms   p95 {p95:>7.1} ms\n",
            "RELEASE->TEXT"
        ));
        out.push_str(&format!(
            "  within the {} ms budget: {}/{n}\n",
            PASTE_BUDGET.as_millis(),
            self.within_budget()
        ));
        let overruns: u64 = self.samples.iter().map(|d| d.overruns).sum();
        if overruns > 0 {
            out.push_str(&format!("  DROPPED AUDIO: {overruns} samples\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(total: f64) -> Dictation {
        Dictation {
            total_ms: total,
            stt_ms: Some(total * 0.9),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_measured_is_none_not_zero() {
        // A pipeline that skipped a stage must not look like one that ran it
        // in no time.
        let l = Latencies::new();
        assert_eq!(l.p50(|d| d.total_ms.into()), None);
        assert!(l.is_empty());
    }

    #[test]
    fn a_stage_that_never_ran_reports_nothing() {
        let mut l = Latencies::new();
        l.record(at(100.0));
        l.record(at(200.0));
        assert_eq!(
            l.p50(|d| d.tier1_ms),
            None,
            "no dictation here waited on cleanup"
        );
        assert!(l.p50(|d| Some(d.total_ms)).is_some());
    }

    #[test]
    fn one_sample_is_its_own_median() {
        let mut l = Latencies::new();
        l.record(at(127.0));
        assert_eq!(l.p50(|d| Some(d.total_ms)), Some(127.0));
        assert_eq!(l.p95(|d| Some(d.total_ms)), Some(127.0));
    }

    #[test]
    fn the_percentiles_are_the_real_readings_from_this_machine() {
        // 78, 119, 127, 153, 267 -- the five real dictations measured on
        // 2026-09-08. Nearest-rank, so p50 is the middle reading and p95 is
        // the worst, not an interpolation between them.
        let mut l = Latencies::new();
        for ms in [153.0, 78.0, 267.0, 119.0, 127.0] {
            l.record(at(ms));
        }
        assert_eq!(l.p50(|d| Some(d.total_ms)), Some(127.0));
        assert_eq!(l.p95(|d| Some(d.total_ms)), Some(267.0));
    }

    #[test]
    fn a_stage_is_averaged_only_over_the_dictations_that_ran_it() {
        let mut l = Latencies::new();
        l.record(Dictation {
            total_ms: 100.0,
            paste_ms: Some(150.0),
            ..Default::default()
        });
        l.record(Dictation {
            total_ms: 100.0,
            paste_ms: None, // printed instead of pasted
            ..Default::default()
        });
        assert_eq!(
            l.p50(|d| d.paste_ms),
            Some(150.0),
            "the dictation that did not paste must not drag the paste median down"
        );
    }

    #[test]
    fn the_paste_budget_is_300_ms() {
        assert_eq!(PASTE_BUDGET.as_millis(), 300);
        assert!(at(267.0).within_budget());
        assert!(!at(301.0).within_budget());

        let mut l = Latencies::new();
        for ms in [78.0, 267.0, 400.0] {
            l.record(at(ms));
        }
        assert_eq!(l.within_budget(), 2);
    }

    #[test]
    fn a_timing_stored_before_the_cloud_stage_existed_still_reads() {
        // History keeps this struct as JSON. A row written by an earlier
        // build has no cloud field and must read as "did not run".
        let old = r#"{"audio_ms":1200,"open_ms":12.0,"engine_ms":8.0,"vad_ms":null,
            "stt_ms":150.0,"tier0_ms":4.0,"tier1_ms":null,"paste_ms":6.0,
            "total_ms":180.0,"overruns":0}"#;
        let d: Dictation = serde_json::from_str(old).expect("an old row reads");
        assert_eq!(d.cloud_ms, None);
        assert_eq!(d.stt_ms, Some(150.0));
    }

    #[test]
    fn a_local_timing_is_stored_without_a_cloud_field() {
        // History keeps this JSON for every dictation. A local one should not
        // grow bytes for a stage it never has.
        let local = serde_json::to_string(&at(127.0)).unwrap();
        assert!(!local.contains("cloud_ms"), "{local}");
        let cloud = serde_json::to_string(&Dictation {
            cloud_ms: Some(2_400.0),
            ..at(2_600.0)
        })
        .unwrap();
        assert!(cloud.contains("\"cloud_ms\":2400.0"), "{cloud}");
    }

    #[test]
    fn the_cloud_stage_is_reported_only_for_dictations_that_sent_a_request() {
        let mut local = Latencies::new();
        local.record(at(127.0));
        assert!(
            !local.report().contains("cloud"),
            "a local session's breakdown is unchanged: {}",
            local.report()
        );

        let mut cloud = Latencies::new();
        cloud.record(Dictation {
            cloud_ms: Some(2_400.0),
            ..at(2_600.0)
        });
        let report = cloud.report();
        assert!(report.contains("cloud"), "got: {report}");
        assert!(report.contains("2400.0"), "got: {report}");
    }

    #[test]
    fn the_report_says_how_many_samples_it_is_built_from() {
        // A p95 over five dictations is not a p95. A table that hides n
        // invites it being quoted as one, which is how the 127 ms fixture
        // figure ended up in docs/benchmarks.md reading like a live number.
        let mut l = Latencies::new();
        l.record(at(127.0));
        let report = l.report();
        assert!(report.contains('1'), "should state the count: {report}");
        assert!(
            report.to_lowercase().contains("dictation"),
            "should say what was counted: {report}"
        );
    }
}
