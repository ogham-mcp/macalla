// SPDX-License-Identifier: MIT
use super::*;

/// The shipped seed, built from what Apple SpeechTranscriber actually got
/// wrong on 2026-09-08.
fn seed() -> Dictionary {
    Dictionary::from_json(include_str!("../../../../fixtures/dictionary/seed.json"))
        .expect("seed must parse")
}

#[test]
fn the_seed_file_parses_and_is_not_empty() {
    assert!(
        seed().len() >= 10,
        "the 0.1 exit criteria ask for ten seeded terms"
    );
}

#[test]
fn the_terms_no_engine_could_produce_are_corrected() {
    // Ogham defeated both engines on both human takes and the synthetic
    // fixtures: OM, Om, Ohm, Ogam. Four attempts, zero successes, for the name
    // of the project family. Supabase and pgvector failed everywhere too.
    let d = seed();
    for (heard, want) in [
        ("OM", "Ogham"),
        ("Om", "Ogham"),
        ("Ohm", "Ogham"),
        ("Ogam", "Ogham"),
        ("Superbase", "Supabase"),
        ("Super base", "Supabase"),
        ("PG vector", "pgvector"),
        ("terror grunt", "Terragrunt"),
        ("postgrass", "Postgres"),
        ("Cube CTL", "kubectl"),
        ("kubectrol", "kubectl"),
        ("tail scale", "Tailscale"),
        ("LXE", "LXC"),
        ("V-Net", "VNet"),
    ] {
        assert_eq!(
            d.correct(heard),
            want,
            "{heard:?} should correct to {want:?}"
        );
    }
}

#[test]
fn the_ambiguous_mishearings_are_deliberately_not_corrected() {
    // Observed, but not seeded. "parcel" and "passer" are ordinary words, so a
    // rule would break "send me the parcel". PostgreSQL is a legitimate name
    // rather than a mishearing. The homophones need engine context, not a
    // rewrite -- both spellings are real English.
    let d = seed();
    for s in [
        "send me the parcel",
        "the passer by",
        "we run PostgreSQL",
        "count the cash",
        "the kitchen sink",
    ] {
        assert_eq!(
            d.correct(s),
            s,
            "{s:?} was altered and should not have been"
        );
    }
}

#[test]
fn every_real_mishearing_is_corrected() {
    // Each pair is something an engine actually produced, against what it
    // should have been. This is an exit criterion for 0.1.
    let d = seed();
    for (heard, want) in [
        ("Terra Grunt", "Terragrunt"),
        ("azure", "Azure"),
        ("V net", "VNet"),
        ("cube control", "kubectl"),
        ("postgress", "Postgres"),
        ("PG vector", "pgvector"),
        ("embodings", "embeddings"),
        ("Superbase", "Supabase"),
        ("tail scale", "Tailscale"),
        ("Ogam", "Ogham"),
    ] {
        assert_eq!(
            d.correct(heard),
            want,
            "{heard:?} should correct to {want:?}"
        );
    }
}

#[test]
fn a_case_sensitive_term_fires_only_on_an_input_written_as_listed() {
    // "om" is an ordinary word, and Ogham lists "OM" and "Om", not "om".
    let d = seed();
    assert_eq!(d.correct("say om quietly"), "say om quietly");
    assert_eq!(d.correct("OM"), "Ogham");
    // Lower case is listed where it is not an ordinary word, so these fire.
    assert_eq!(
        d.correct("we deployed terraform to azure"),
        "we deployed Terraform to Azure"
    );
    // A case-sensitive term that is an ordinary word in another case
    // leaves that word alone.
    let mut file: TermFile =
        serde_json::from_str(include_str!("../../../../fixtures/dictionary/seed.json")).unwrap();
    file.terms.push(Term {
        written: "IT".into(),
        spoken: vec!["I T".into()],
        case_sensitive: true,
        threshold: DEFAULT_THRESHOLD,
    });
    let with_it = Dictionary::new(&file);
    assert_eq!(with_it.correct("is it ready"), "is it ready");
    assert_eq!(with_it.correct("It is ready"), "It is ready");
    assert_eq!(with_it.correct("the I T team"), "the IT team");
}

#[test]
fn a_mishearing_capitalised_to_open_a_sentence_is_still_corrected() {
    // Engines and models capitalise a sentence's first word. Parakeet
    // produced "Cube control"; the seed lists "cube control".
    let d = seed();
    assert_eq!(
        d.correct("Cube control showed the pods"),
        "kubectl showed the pods"
    );
    assert_eq!(d.correct("Tail scale is up"), "Tailscale is up");
    assert_eq!(d.correct("Terraform applied"), "Terraform applied");
    // A capital allows nothing more for a form already capitalised.
    assert_eq!(d.correct("Om is a word"), "Ogham is a word");
    assert_eq!(d.correct("om is a word"), "om is a word");
}

#[test]
fn a_case_sensitive_terms_case_variants_survive_an_edit() {
    let mut f = file_with("Terraform", &["terra form"]);
    let term = Term {
        written: "Terraform".into(),
        spoken: vec![
            "terra form".into(),
            "terraform".into(),
            " TerraForm ".into(),
            "terraform".into(),
            "Terraform".into(),
        ],
        case_sensitive: true,
        threshold: DEFAULT_THRESHOLD,
    };
    f.update("Terraform", term).unwrap();
    // Exact duplicates and the written form itself go; case variants stay.
    assert_eq!(
        f.terms[0].spoken,
        vec!["terra form", "terraform", "TerraForm"]
    );
    f.add("Terraform", &["TERRAFORM".into(), "TerraForm".into()])
        .unwrap();
    assert_eq!(
        f.terms[0].spoken,
        vec!["terra form", "terraform", "TerraForm", "TERRAFORM"]
    );
}

#[test]
fn an_imported_term_keeps_its_case_sensitivity_and_threshold() {
    let theirs = TermFile {
        terms: vec![Term {
            written: "Azure".into(),
            spoken: vec!["azure".into(), "AZURE".into()],
            case_sensitive: true,
            threshold: 0.95,
        }],
    };
    let mut mine = TermFile::default();
    assert_eq!(mine.merge(&theirs), 1);
    let azure = mine.get("Azure").unwrap();
    assert!(azure.case_sensitive);
    assert_eq!(azure.threshold, 0.95);
    assert_eq!(azure.spoken, vec!["azure", "AZURE"]);
}

#[test]
fn corrections_work_inside_a_real_sentence() {
    let d = seed();
    assert_eq!(
        d.correct("I ran Terra Grunt against the azure landing zone"),
        "I ran Terragrunt against the Azure landing zone"
    );
    assert_eq!(
        d.correct("Superbase runs on the Proxmox LXC over tail scale"),
        "Supabase runs on the Proxmox LXC over Tailscale"
    );
}

#[test]
fn trailing_punctuation_survives_a_correction() {
    let d = seed();
    assert_eq!(d.correct("we use postgress."), "we use Postgres.");
    assert_eq!(d.correct("is it postgress?"), "is it Postgres?");
}

#[test]
fn a_term_already_correct_is_left_alone() {
    let d = seed();
    for w in ["Terragrunt", "kubectl", "Postgres", "Supabase", "Tailscale"] {
        assert_eq!(d.correct(w), w, "{w:?} was already right");
    }
}

#[test]
fn longest_ngram_wins() {
    // "cube control" is two words and must beat any single-word match on
    // "cube" or "control".
    let d = seed();
    assert_eq!(d.correct("run cube control now"), "run kubectl now");
}

#[test]
fn ordinary_words_are_not_dragged_into_technical_terms() {
    // The failure mode that would make this feature unusable: a dictionary of
    // technical terms rewriting normal English.
    let d = seed();
    for s in [
        "the answer is obvious",
        "we should measure it first",
        "a cube of ice",
        "please scale the image",
        "the vector of the attack",
    ] {
        assert_eq!(d.correct(s), s, "{s:?} was altered");
    }
}

#[test]
fn an_empty_dictionary_is_a_no_op() {
    let d = Dictionary::new(&TermFile::default());
    assert!(d.is_empty());
    assert_eq!(d.correct("anything at all"), "anything at all");
}

#[test]
fn written_forms_are_exposed_for_engine_bias_and_the_tier1_prompt() {
    let d = seed();
    let forms = d.written_forms();
    assert!(forms.contains(&"kubectl"));
    assert!(forms.contains(&"Terragrunt"));
    assert_eq!(forms.len(), d.len());
}

// --- The dictionary's last word, after Tier 1 ---

fn para(text: &str) -> Vec<Block> {
    vec![Block::Paragraph { text: text.into() }]
}

#[test]
fn settle_restores_a_written_forms_case_even_when_the_spelling_was_already_right() {
    // Tier 1's own sentence case did this, the same way Tier 0's does.
    let d = seed();
    assert_eq!(
        d.settle(&para("Kubectl showed the pods.")),
        para("kubectl showed the pods.")
    );
    assert_eq!(
        d.settle(&para("the Vnet was valid")),
        para("the VNet was valid")
    );
    // A casing no ordinary word has is the term re-cased, and is put back
    // even for a case-sensitive term that does not list it.
    assert_eq!(
        d.settle(&para("We use PGVector on the VNET with TERRAFORM.")),
        para("We use pgvector on the VNet with Terraform.")
    );
}

#[test]
fn settle_corrects_a_free_form_replys_respelling() {
    // A person's own prompt may change up to 40% of the words, so a free-form reply can respell a seeded term outright, not just
    // re-case it.
    let d = seed();
    assert_eq!(
        d.settle(&para("I ran Terra Grunt against it.")),
        para("I ran Terragrunt against it.")
    );
}

#[test]
fn settle_leaves_a_sentence_that_is_already_right_byte_identical() {
    let d = seed();
    let blocks = para("Clear the cash before you count the cash.");
    assert_eq!(d.settle(&blocks), blocks);
}

#[test]
fn settle_applied_twice_equals_settle_applied_once() {
    let d = seed();
    let once = d.settle(&para("Kubectl showed the pods, then the Vnet was valid."));
    let twice = d.settle(&once);
    assert_eq!(once, twice);
}

#[test]
fn settle_keeps_each_blocks_kind() {
    let d = seed();
    let blocks = vec![Block::ListItem {
        text: "Kubectl showed the pods.".into(),
        ordered: true,
    }];
    assert_eq!(
        d.settle(&blocks),
        vec![Block::ListItem {
            text: "kubectl showed the pods.".into(),
            ordered: true
        }]
    );
}

#[test]
fn settle_is_a_no_op_for_an_empty_dictionary() {
    let d = Dictionary::new(&TermFile::default());
    let blocks = para("Kubectl showed the pods.");
    assert_eq!(d.settle(&blocks), blocks);
}

/// Replay of a real Tier 1 run (`bench/tier1-out.json`), pinned to the exact
/// output of every one of its 20 lines: 18 stay as Tier 1 wrote them, and two
/// -- "tailscale"/"proxmox"/"lxc" already right but wrongly cased, and
/// "terraform"/"vnet" the same -- get `settle`'s last word. Prints every line
/// that changed, so a future diff against this fixture is easy to read even
/// though the assertion pins the whole string.
#[test]
fn settle_replays_a_real_tier1_run_with_exact_expected_output() {
    #[derive(serde::Deserialize)]
    struct Row {
        tier1: String,
    }
    let rows: Vec<Row> = serde_json::from_str(include_str!("../../../../bench/tier1-out.json"))
        .expect("bench/tier1-out.json must parse");
    let expected = [
        "The thing is, we need to ship the parser by Friday.",
        "I was saying that the latency budget is three hundred milliseconds.",
        "So basically, uh, what I mean is the VAD gates the stream, not the other way round.",
        "Okay, we should probably run Terragrunt against the Azure landing zone first.",
        "The PostgreSQL database uses pgvector for the embeddings.",
        "I think we need to check the kubectl output before deploying.",
        "So the thing is that Tailscale runs on the Proxmox LXC container.",
        "basically the seam pass rejoins segments at assembly time",
        "Right, we need to measure the p value ninety-five not just the median.",
        "The clipboard needs to be restored after we paste the text.",
        "Terraform applied the VNet and then the hcl was valid",
        "like i said, the audio callback should only copy samples",
        "The whisper model is much slower than Parakeet on this machine.",
        "You know, the accessibility permission cannot be granted programmatically.",
        "The ring buffer sits between capture and the vad.",
        "so i mean the the notarisation step runs in ci not locally",
        "We should probably cache the phonetic keys at load time.",
        "The tray menu has exactly five items and nothing else.",
        "The Supabase instance is behind the tailnet, not the public internet.",
        "like the enhancement never blocks the paste that is the whole point",
    ];
    assert_eq!(
        rows.len(),
        expected.len(),
        "the recorded replay run has 20 lines"
    );
    let d = seed();
    for (row, want) in rows.iter().zip(expected.iter()) {
        let got = d.settle(&para(&row.tier1));
        if got != para(&row.tier1) {
            println!("changed: {:?} -> {:?}", row.tier1, got[0].text());
        }
        assert_eq!(got, para(want), "input: {:?}", row.tier1);
    }
}

/// 500 terms shaped like a real dictionary: varied first letters and lengths,
/// which is what lets the indices prune.
fn realistic_500() -> Dictionary {
    let mut file = TermFile::default();
    let stems = [
        "terra", "kube", "post", "super", "tail", "prox", "cloud", "vault", "grafana", "redis",
        "nginx", "envoy", "istio", "argo", "flux", "helm", "kafka", "pulsar", "consul", "nomad",
        "bicep", "entra", "azure", "lambda", "fargate", "aurora",
    ];
    for i in 0..500 {
        let stem = stems[i % stems.len()];
        file.terms.push(Term {
            written: format!("{stem}{i}"),
            spoken: vec![format!("{stem} {i}")],
            case_sensitive: false,
            threshold: DEFAULT_THRESHOLD,
        });
    }
    Dictionary::new(&file)
}

fn per_call(d: &Dictionary, sentence: &str) -> std::time::Duration {
    let start = std::time::Instant::now();
    for _ in 0..200 {
        let _ = d.correct(sentence);
    }
    start.elapsed() / 200
}

/// The 1 ms budget applies to the shipped build. An unoptimised Levenshtein is
/// roughly an order of magnitude slower, so asserting 1 ms under `cargo test`
/// would either fail honest code or force the budget loose enough to be
/// meaningless. Measured 2026-09-08: 128us per call in release.
const BUDGET: std::time::Duration = if cfg!(debug_assertions) {
    std::time::Duration::from_millis(10)
} else {
    std::time::Duration::from_millis(1)
};

#[test]
fn five_hundred_terms_correct_a_sentence_within_the_budget() {
    // Run `cargo test --release` for the figure that counts.
    let d = realistic_500();
    assert_eq!(d.len(), 500);
    let t = per_call(
        &d,
        "we deployed the service and then ran the benchmarks against it twice",
    );
    assert!(
        t < BUDGET,
        "500 realistic terms took {t:?}, budget {BUDGET:?}"
    );
}

#[test]
fn an_adversarial_dictionary_degrades_but_stays_usable() {
    // Every term sharing a stem and a length band defeats both indices: the
    // first-character guard and the length window prune nothing, so the matcher
    // falls back to scoring most of the dictionary. Not a shape a user produces,
    // but the bound should be known rather than assumed.
    let mut file = TermFile::default();
    for i in 0..500 {
        file.terms.push(Term {
            written: format!("Term{i}"),
            spoken: vec![format!("term {i}")],
            case_sensitive: false,
            threshold: DEFAULT_THRESHOLD,
        });
    }
    let d = Dictionary::new(&file);
    let t = per_call(
        &d,
        "we deployed the service and then ran the benchmarks against it twice",
    );
    assert!(
        t < BUDGET * 3,
        "adversarial 500 took {t:?}, worse than the known bound"
    );
}

#[test]
fn an_ngram_match_never_swallows_an_adjacent_word() {
    // Regression. Soundex ignores vowels and truncates to four characters, so
    // "proxmoxlxc" and "proxmox" share a key. A two-word n-gram then matched a
    // two-word spoken form and deleted "LXC" outright. A deletion the user
    // cannot see is the worst thing this layer can do.
    let d = seed();
    assert_eq!(
        d.correct("Supabase runs on the Proxmox LXC over Tailscale"),
        "Supabase runs on the Proxmox LXC over Tailscale"
    );
    assert_eq!(d.correct("the Proxmox LXC"), "the Proxmox LXC");
}

#[test]
fn a_longer_legitimate_name_is_not_shortened_to_a_seeded_one() {
    // "PostgreSQL" is a real name, not a mishearing of "Postgres".
    let d = seed();
    assert_eq!(d.correct("we run PostgreSQL"), "we run PostgreSQL");
}

#[test]
fn casing_is_corrected_even_when_the_word_is_otherwise_right() {
    // A commercial dictation app has Terragrunt, Terraform and Supabase in its
    // own dictionary and still emitted "TerraGrunt" and "TerraForm". Producing the
    // exact written form is part of the job, not a nicety: a dictionary that
    // returns the wrong casing for a term it contains has not done its work.
    let d = seed();
    assert_eq!(d.correct("TerraGrunt"), "Terragrunt");
    assert_eq!(d.correct("TerraForm"), "Terraform");
    assert_eq!(
        d.correct(
            "I ran TerraGrunt against the Azure landing zone, and then TerraForm applied the VNet"
        ),
        "I ran Terragrunt against the Azure landing zone, and then Terraform applied the VNet"
    );
}

// --- Spelled-out letters ---

#[test]
fn spelled_out_letters_with_spaces_still_match_a_spoken_form() {
    // "Cube CTL" is seeded for kubectl, but an engine sometimes spells the
    // last three letters out with spaces between them instead of running
    // them together.
    let d = seed();
    assert_eq!(d.correct("Cube c t l."), "kubectl.");
    assert_eq!(
        d.correct("we ran cube c t l to check"),
        "we ran kubectl to check"
    );
}

#[test]
fn a_spoken_form_written_with_spelled_out_letters_matches_either_way() {
    let f = file_with("kubectl", &["Cube c t l"]);
    let d = Dictionary::new(&f);
    assert_eq!(d.correct("Cube CTL."), "kubectl.");
    assert_eq!(d.correct("Cube c t l."), "kubectl.");
}

#[test]
fn the_seeds_spelled_out_lxc_form_still_corrects_without_losing_a_word() {
    // Regression partner to `an_ngram_match_never_swallows_an_adjacent_word`:
    // folding "L X C" into one logical word must not repeat the deletion
    // bug that comment is named after.
    let d = seed();
    assert_eq!(d.correct("L X C"), "LXC");
    assert_eq!(
        d.correct("Proxmox L X C over tail scale"),
        "Proxmox LXC over Tailscale"
    );
}

#[test]
fn unmatched_spelled_out_letters_pass_through_untouched() {
    let d = seed();
    assert_eq!(d.correct("I ran a b test"), "I ran a b test");
    assert_eq!(d.correct("U S A"), "U S A");
}

#[test]
fn a_lone_single_letter_beside_a_term_is_not_swallowed() {
    let d = seed();
    assert_eq!(d.correct("I ran cube control now"), "I ran kubectl now");
    assert_eq!(d.correct("a LXE box"), "a LXC box");
}

#[test]
fn letter_names_need_no_special_code() {
    // "Oh lama" is ordinary two-word matching -- neither token is a single
    // letter -- so nothing beyond existing spoken-form matching is needed
    // here; the test exists to prove that, not to add behaviour for it.
    let f = file_with("Ollama", &["Oh lama"]);
    let d = Dictionary::new(&f);
    assert_eq!(d.correct("Oh, lama."), "Ollama.");
}

// --- Editing the term file ---

fn file_with(written: &str, spoken: &[&str]) -> TermFile {
    let mut f = TermFile::default();
    f.add(
        written,
        &spoken.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .unwrap();
    f
}

#[test]
fn adding_a_new_written_form_creates_a_term() {
    let f = file_with("kubectl", &["cube control", "kube control"]);
    assert_eq!(f.terms.len(), 1);
    assert_eq!(f.terms[0].spoken, vec!["cube control", "kube control"]);
}

#[test]
fn adding_to_a_known_written_form_extends_it_ignoring_case() {
    let mut f = file_with("kubectl", &["cube control"]);
    let created = f.add("Kubectl", &["kube cuddle".into()]).unwrap();
    assert!(!created);
    assert_eq!(f.terms.len(), 1);
    assert_eq!(f.terms[0].written, "kubectl", "the first spelling stays");
    assert_eq!(f.terms[0].spoken, vec!["cube control", "kube cuddle"]);
}

#[test]
fn spoken_forms_are_tidied() {
    let f = file_with("Macalla", &[" Makala ", "makala", "Macalla", "", "Macala"]);
    assert_eq!(f.terms[0].spoken, vec!["Makala", "Macala"]);
}

#[test]
fn a_term_needs_a_written_form() {
    let mut f = TermFile::default();
    assert_eq!(f.add("  ", &[]), Err(EditError::EmptyWritten));
}

#[test]
fn updating_can_rename_but_not_onto_another_term() {
    let mut f = file_with("kubectl", &["cube control"]);
    f.add("pgvector", &["pg vector".into()]).unwrap();
    let renamed = Term {
        written: "kubectl CLI".into(),
        spoken: vec!["cube control".into()],
        case_sensitive: false,
        threshold: DEFAULT_THRESHOLD,
    };
    f.update("kubectl", renamed).unwrap();
    assert!(f.get("kubectl CLI").is_some());
    assert!(f.get("kubectl").is_none());

    let collides = Term {
        written: "PGVECTOR".into(),
        spoken: vec![],
        case_sensitive: false,
        threshold: DEFAULT_THRESHOLD,
    };
    assert_eq!(
        f.update("kubectl CLI", collides),
        Err(EditError::Duplicate("PGVECTOR".into()))
    );
    assert_eq!(
        f.update(
            "nothing",
            Term {
                written: "x".into(),
                spoken: vec![],
                case_sensitive: false,
                threshold: DEFAULT_THRESHOLD
            }
        ),
        Err(EditError::Unknown("nothing".into()))
    );
}

#[test]
fn removing_is_by_written_form_ignoring_case() {
    let mut f = file_with("kubectl", &[]);
    assert!(f.remove("KUBECTL"));
    assert!(!f.remove("kubectl"));
    assert!(f.terms.is_empty());
}

#[test]
fn merging_adds_and_extends_but_never_removes() {
    let mut mine = file_with("kubectl", &["cube control"]);
    mine.add("Ogham", &["ogam".into()]).unwrap();
    let mut theirs = file_with("kubectl", &["kube cuddle"]);
    theirs.add("pgvector", &["pg vector".into()]).unwrap();
    let changed = mine.merge(&theirs);
    assert_eq!(changed, 2);
    assert_eq!(mine.terms.len(), 3);
    assert_eq!(
        mine.get("kubectl").unwrap().spoken,
        vec!["cube control", "kube cuddle"]
    );
    assert!(mine.get("Ogham").is_some(), "an import never removes");
    // Merging the same file again changes nothing.
    assert_eq!(mine.merge(&theirs), 0);
}

#[test]
fn a_plain_word_list_imports_as_written_forms_with_nothing_spoken() {
    let file = TermFile::parse("Terragrunt\nkubectl\n\n# a comment\nOgham\n").unwrap();
    let written: Vec<&str> = file.terms.iter().map(|t| t.written.as_str()).collect();
    assert_eq!(written, ["Terragrunt", "kubectl", "Ogham"]);
    assert!(file.terms.iter().all(|t| t.spoken.is_empty()));
}

#[test]
fn heard_written_pairs_import_as_spoken_forms_and_a_header_is_skipped() {
    let csv =
        "heard,written\nterra grunt,Terragrunt\n\"cube control\",\"kubectl\"\ncube CTL,kubectl\n";
    let file = TermFile::parse(csv).unwrap();
    assert_eq!(file.terms.len(), 2);
    assert_eq!(file.terms[0].written, "Terragrunt");
    assert_eq!(file.terms[0].spoken, ["terra grunt"]);
    assert_eq!(file.terms[1].written, "kubectl");
    assert_eq!(file.terms[1].spoken, ["cube control", "cube CTL"]);
}

#[test]
fn tab_separated_pairs_and_a_mixed_list_both_work() {
    let file = TermFile::parse("postgres\tPostgreSQL\nProxmox\n").unwrap();
    assert_eq!(file.terms[0].written, "PostgreSQL");
    assert_eq!(file.terms[0].spoken, ["postgres"]);
    assert_eq!(file.terms[1].written, "Proxmox");
}

#[test]
fn json_is_still_the_native_shape() {
    let file =
        TermFile::parse("{\"terms\":[{\"written\":\"Ogham\",\"spoken\":[\"ohm\"]}]}").unwrap();
    assert_eq!(file.terms[0].written, "Ogham");
    assert_eq!(file.terms[0].spoken, ["ohm"]);
    assert!(TermFile::parse("{not json").is_err());
}

#[test]
fn a_pair_with_no_written_form_is_refused_with_its_line_number() {
    let err = TermFile::parse("ok\nheard,\n").unwrap_err();
    assert!(err.to_string().contains("line 2"), "{err}");
}

// --- Punctuation wrapping a matched term, both sides ---

#[test]
fn a_matched_term_keeps_the_punctuation_that_wrapped_it() {
    // Only the trailing side used to be reattached, so a leading paren, a
    // backtick, bold, a quote or a bracket was dropped -- and `settle` now
    // runs `correct` over a model's own reply, which wraps terms exactly
    // this way.
    let d = seed();
    assert_eq!(d.correct("(postgress)"), "(Postgres)");
    assert_eq!(
        d.correct("`kubectl` showed the pods"),
        "`kubectl` showed the pods"
    );
    assert_eq!(
        d.correct("The **Proxmox** LXC container"),
        "The **Proxmox** LXC container"
    );
    assert_eq!(
        d.correct("say \"terra grunt\" now"),
        "say \"Terragrunt\" now"
    );
    assert_eq!(d.correct("[V net] config"), "[VNet] config");
}

#[test]
fn settle_keeps_the_punctuation_that_wrapped_a_term_too() {
    let d = seed();
    assert_eq!(
        d.settle(&para("Run `kubectl` to see the pods.")),
        para("Run `kubectl` to see the pods.")
    );
}

#[test]
fn a_lettered_list_in_a_model_reply_stays_intact() {
    // "A.", "B.", "C." merge into one spelled-out run the same way "L X C"
    // does; regression against that run being read as a dictionary term.
    let d = seed();
    assert_eq!(
        d.settle(&para("The options are A. B. C. Pick one.")),
        para("The options are A. B. C. Pick one.")
    );
}

// `correction_offer`: whether a hand correction looks like one mishearing
// worth teaching the dictionary. Table-driven: every edge of the rule, each
// as its own row so a failure names the case, not just "some row failed".

fn offer(heard: &str, written: &str) -> Option<Offer> {
    Some(Offer {
        heard: heard.to_string(),
        written: written.to_string(),
    })
}

#[test]
fn correction_offer_rule_edges() {
    for (label, old, new, want) in [
        (
            "a one-word swap is offered",
            "I met Bob at noon",
            "I met Bill at noon",
            offer("Bob", "Bill"),
        ),
        (
            "a three-word run is offered",
            "Reach out to Cube Control Panel",
            "Reach out to Kube Konnect Module",
            offer("Cube Control Panel", "Kube Konnect Module"),
        ),
        (
            "a four-word run is not offered",
            "Reach out to Cube Control Panel Extra",
            "Reach out to Kube Konnect Module Other",
            None,
        ),
        (
            "two separate changes, far enough apart to span more than \
             three words, are not offered",
            "I met Bob and then Jane went home",
            "I met Bill and then Jill went home",
            None,
        ),
        (
            "two changes with one unchanged word between them ARE offered, \
             as one run: the rule does not try to detect two edits, and the \
             person confirms in TermForm before anything is saved",
            "the cat sat",
            "a cat sits",
            offer("the cat sat", "a cat sits"),
        ),
        (
            "an insertion alone is not offered: nothing was misheard",
            "Call the client",
            "Call the important client",
            None,
        ),
        (
            "a deletion alone is not offered: nothing was misheard",
            "Call the important client",
            "Call the client",
            None,
        ),
        (
            "a punctuation-only fix is not offered",
            "Ping Jira.",
            "Ping Jira,",
            None,
        ),
        (
            "a case-only fix is offered: fixing how a name is written is \
             one of the dictionary's main uses",
            "use github for this",
            "use GitHub for this",
            offer("github", "GitHub"),
        ),
        (
            "trimming compares with edge punctuation stripped: the comma \
             after \"Hi\" does not stop it being common to both",
            "Hi, bob here",
            "Hi bill here",
            offer("bob", "bill"),
        ),
        (
            "a swap at the very start of the turn is offered",
            "Bob called this morning",
            "Bill called this morning",
            offer("Bob", "Bill"),
        ),
        (
            "a swap in the middle of the turn is offered",
            "Please call Bob today",
            "Please call Bill today",
            offer("Bob", "Bill"),
        ),
        (
            "a swap at the very end of the turn is offered",
            "Called about Bob",
            "Called about Bill",
            offer("Bob", "Bill"),
        ),
        (
            "no change at all is not offered",
            "Nothing changed here",
            "Nothing changed here",
            None,
        ),
        (
            "a sentence's full stop is not part of the offer",
            "Let's start with the roadmap for next quarter.",
            "Let's start with the roadmap for next Q3.",
            offer("quarter", "Q3"),
        ),
        (
            "punctuation in the middle of a run stays",
            "Talk to Mr. Bob now",
            "Talk to Ms. Bill now",
            offer("Mr. Bob", "Ms. Bill"),
        ),
        (
            "ASCII quotes around a name are stripped",
            "Ping \"Jira\" now",
            "Ping \"Trello\" now",
            offer("Jira", "Trello"),
        ),
        (
            "curly quotes are stripped too, not just the ASCII set",
            "Ping Jira now",
            "Ping \u{201C}Macalla\u{201D} now",
            offer("Jira", "Macalla"),
        ),
        (
            "an en dash swapped for an em dash is not offered: both strip \
             to nothing, so the whole span reads as unchanged",
            "Section 1 \u{2013} Overview",
            "Section 1 \u{2014} Overview",
            None,
        ),
        (
            "an em dash leading straight into a word is stripped from the \
             offer, not just recognised as punctuation",
            "x \u{2014}bob",
            "x bill",
            offer("bob", "bill"),
        ),
        (
            "the ellipsis character is stripped from the offer the same way",
            "x bob\u{2026}",
            "x bill",
            offer("bob", "bill"),
        ),
    ] {
        assert_eq!(correction_offer(old, new), want, "{label}");
    }
}

#[test]
fn correction_offer_strips_a_run_s_own_edge_punctuation_but_not_its_middle() {
    // The comparison that decides a run differs strips each word's edge
    // punctuation for that check alone (`stripped`); the offer itself joins
    // the run and strips only the WHOLE run's own two edges, so "quarter."
    // offers "quarter" (the full stop is the sentence's, not the term's)
    // while an internal hyphen, not at the run's edge, survives.
    assert_eq!(
        correction_offer("Reach kube-control.", "Reach Kubectl."),
        offer("kube-control", "Kubectl")
    );
}

#[test]
fn correction_offer_offers_nothing_when_stripping_empties_a_run() {
    // A run that is only edge punctuation once stripped -- "..." strips to
    // nothing -- leaves nothing to teach, even though "---" (a hyphen is
    // not in `EDGE_PUNCTUATION`) survives, so the two stripped forms differ
    // and the trim does not remove them.
    assert_eq!(correction_offer("Say ... now", "Say --- now"), None);
}

#[test]
fn correction_offer_offers_nothing_when_a_run_has_no_alphanumeric_character() {
    // "..." and "(((" each strip fully at the run's own two edges, but the
    // space between them (from `join(" ")`) is not punctuation, so the run
    // does not go empty -- it goes to a bare space, which still has no
    // letter or digit in it. Distinct from the plain-emptiness case above:
    // this is what actually rejects it.
    assert_eq!(correction_offer("See ... ((( stuff", "See fix stuff"), None);
}

#[test]
fn correction_offer_caps_a_run_by_characters_too_not_just_words() {
    // CJK text has no whitespace, so a long phrase is one `split_whitespace`
    // "word" and slips past the word-count cap entirely; the character cap
    // (`MAX_OFFER_CHARS`) is what actually stops a whole sentence being
    // offered as if it were one short run.
    let old = "字".repeat(MAX_OFFER_CHARS + 1);
    let new = "词".repeat(MAX_OFFER_CHARS + 1);
    assert_eq!(correction_offer(&old, &new), None);
}

#[test]
fn correction_offer_caps_the_stripped_run_not_the_raw_one() {
    // The character cap is checked after stripping, not before: a
    // `MAX_OFFER_CHARS`-character term followed by a full stop is exactly
    // `MAX_OFFER_CHARS` characters once offered, and must not be rejected
    // as one character too many for a full stop that is not part of it.
    let a = "a".repeat(MAX_OFFER_CHARS);
    let b = "b".repeat(MAX_OFFER_CHARS);
    let old = format!("Reach {a}.");
    let new = format!("Reach {b}.");
    assert_eq!(correction_offer(&old, &new), offer(&a, &b));
}
