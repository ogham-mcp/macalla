// SPDX-License-Identifier: MIT
use super::*;
use crate::enhance::Opening;

fn en() -> Tier0Rules {
    Tier0Rules::new(Some("en-US"))
}

fn plain(text: &str) -> String {
    let blocks = en().process(text, None);
    blocks
        .iter()
        .map(|b| b.text().to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn unambiguous_fillers_are_removed_anywhere() {
    assert_eq!(plain("um we need uh to ship it"), "We need to ship it");
}

#[test]
fn ambiguous_words_are_only_removed_when_they_open_an_utterance() {
    // "so" leads, and goes.
    assert_eq!(
        plain("so the thing is we need to ship"),
        "The thing is we need to ship"
    );
    // The same word mid-sentence is load-bearing and must survive.
    assert_eq!(
        plain("we shipped it so the tests pass"),
        "We shipped it so the tests pass"
    );
}

#[test]
fn like_is_never_deleted_at_all() {
    // "like" is ambiguous even at the start of an utterance: dropping it from
    // "like I said" is right, but from "like a boss" is wrong. Tier 0 declines
    // and leaves it to Tier 1, which has the sentence context to judge.
    assert_eq!(plain("i like it"), "I like it");
    assert_eq!(plain("like i said it works"), "Like i said it works");
}

#[test]
fn stuttered_phrases_collapse() {
    assert_eq!(
        plain("we need to we need to ship the the parser"),
        "We need to ship the parser"
    );
}

#[test]
fn a_deliberate_repeat_loses_a_copy_because_repeats_collapse_by_pairs() {
    // Pinned as documented in docs/known-behaviours.md: the words alone
    // cannot tell a deliberate repeat from a stumble. Three copies of a run
    // of up to three words become two.
    assert_eq!(
        plain("one two three one two three one two three testing"),
        "One two three one two three testing"
    );
    // A single word collapses only when it is on the stutterable list.
    assert_eq!(
        plain("azure azure terraform azure"),
        "Azure azure terraform azure"
    );
    // Punctuation between the copies keeps them all.
    assert_eq!(
        plain("one two three comma one two three"),
        "One two three, one two three"
    );
}

#[test]
fn emphasis_is_not_mistaken_for_a_stutter() {
    // "very very good" is emphasis. Only words on the explicit stutterable
    // list collapse -- a word-length heuristic got this wrong, because "very"
    // is four characters.
    assert_eq!(plain("that was very very good"), "That was very very good");
}

#[test]
fn spoken_punctuation_attaches_to_the_previous_word() {
    assert_eq!(
        plain("the budget is three hundred milliseconds period"),
        "The budget is three hundred milliseconds."
    );
    assert_eq!(
        plain("first comma we measure then we tune"),
        "First, we measure then we tune"
    );
}

#[test]
fn a_terminator_capitalises_the_next_sentence() {
    assert_eq!(
        plain("we shipped it period the tests pass"),
        "We shipped it. The tests pass"
    );
}

#[test]
fn new_paragraph_splits_blocks() {
    let blocks = en().process("we shipped it new paragraph the tests pass", None);
    assert_eq!(blocks.len(), 2);
    assert!(matches!(blocks[0], Block::Paragraph { .. }));
    assert_eq!(blocks[1].text(), "The tests pass");
}

#[test]
fn heading_classifies_the_following_block() {
    let blocks = en().process(
        "heading resource budgets new paragraph idle rss is low",
        None,
    );
    assert!(
        matches!(blocks[0], Block::Heading { .. }),
        "got {:?}",
        blocks[0]
    );
    assert_eq!(blocks[0].text(), "Resource budgets");
    assert!(matches!(blocks[1], Block::Paragraph { .. }));
}

#[test]
fn a_spoken_list_becomes_list_items() {
    let blocks = en().process(
        "new list review the spec next item run the benchmarks next item write it up",
        None,
    );
    assert_eq!(blocks.len(), 3);
    for b in &blocks {
        assert!(
            matches!(b, Block::ListItem { ordered: false, .. }),
            "got {b:?}"
        );
    }
    assert_eq!(blocks[0].text(), "Review the spec");
    assert_eq!(blocks[2].text(), "Write it up");
}

#[test]
fn a_numbered_list_is_ordered() {
    let blocks = en().process(
        "numbered list review the spec next item run the benchmarks",
        None,
    );
    assert!(blocks
        .iter()
        .all(|b| matches!(b, Block::ListItem { ordered: true, .. })));
}

#[test]
fn longest_match_wins_so_new_paragraph_beats_new() {
    // "new" alone is not a command; only the full phrases are.
    let blocks = en().process("we need a new parser", None);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].text(), "We need a new parser");
}

#[test]
fn technical_terms_pass_through_untouched() {
    // Tier 0 must never rewrite vocabulary -- that is the dictionary's job.
    assert_eq!(
        plain("um run terragrunt against the azure landing zone"),
        "Run terragrunt against the azure landing zone"
    );
}

#[test]
fn a_stub_language_passes_text_through_unchanged() {
    let de = Tier0Rules::new(Some("it-IT"));
    let blocks = de.process("äh wir müssen das äh liefern", None);
    assert_eq!(blocks.len(), 1);
    // No German rules are populated, so nothing is removed. Better to leave
    // text alone than apply English rules to German speech.
    assert!(
        blocks[0].text().contains("äh"),
        "got {:?}",
        blocks[0].text()
    );
}

#[test]
fn an_unknown_language_does_not_get_english_rules() {
    let fr = Tier0Rules::new(Some("fr-FR"));
    let blocks = fr.process("um we need to ship", None);
    assert!(blocks[0].text().to_lowercase().contains("um"));
}

#[test]
fn empty_input_produces_no_blocks() {
    assert!(en().process("", None).is_empty());
    assert!(en().process("   ", None).is_empty());
}

#[test]
fn a_block_of_only_fillers_is_dropped_entirely() {
    assert!(en().process("um uh erm", None).is_empty());
}

#[test]
fn the_real_spike_c_case_is_handled_without_damage() {
    // Tier 1 turned this into "We deployed to AWS, and the cube control..." --
    // dropping "as well" and inserting "the". Tier 0 must not.
    assert_eq!(
        plain("we deployed to aws as well and cube control showed the pods running"),
        "We deployed to aws as well and cube control showed the pods running"
    );
}

#[test]
fn a_deleted_filler_does_not_strand_its_punctuation() {
    // "Yesterday, um, I think" must not render as "Yesterday, , I think".
    // ("well" would be a poor subject here: it is itself a sentence-initial
    // filler and gets removed first.) The failure mode is from sebsto/wispr's
    // FillerWordCleaner; this is our token-stream equivalent of the fix.
    assert_eq!(
        plain("yesterday comma um comma i think"),
        "Yesterday, i think"
    );
}

#[test]
fn leading_punctuation_is_dropped_not_rendered() {
    assert_eq!(plain("um comma we shipped it"), "We shipped it");
}

#[test]
fn the_extended_filler_list_covers_the_unambiguous_sounds() {
    for f in [
        "um", "uh", "ah", "oh", "eh", "er", "erm", "hmm", "hm", "mhm",
    ] {
        let got = plain(&format!("{f} we shipped it"));
        assert_eq!(got, "We shipped it", "filler {f:?} survived as {got:?}");
    }
}

#[test]
fn an_unknown_language_fails_closed_rather_than_defaulting_to_english() {
    // "um" is Portuguese for "a/an" and German for "at/around". Defaulting an
    // unknown language to English would delete a real word. Principle adopted
    // from Handy's audio_toolkit/text.rs: skip rather than guess.
    let unknown = Tier0Rules::new(None);
    let blocks = unknown.process("um carro rapido", None);
    assert_eq!(blocks[0].text(), "Um carro rapido");
}

#[test]
fn cross_language_fillers_are_gated_but_universal_ones_are_not() {
    // With English named, "um" is safe and goes.
    assert_eq!(plain("um we shipped it"), "We shipped it");
    // "hmm" is not a word in any language we output, so it goes even with no
    // language evidence at all. This is why the universal list is global and
    // not per-language: a Portuguese dictation containing "uh" should still
    // lose it, and it did not until the combinatorial test caught it.
    let unknown = Tier0Rules::new(None);
    assert_eq!(
        unknown.process("hmm we shipped it", None)[0].text(),
        "We shipped it"
    );
    // ...but "um" survives without evidence, because it is Portuguese "a/an".
    assert_eq!(
        unknown.process("um carro rapido", None)[0].text(),
        "Um carro rapido"
    );
}

#[test]
fn universal_fillers_are_removed_in_every_language() {
    for lang in ["en", "de", "es", "fr", "it", "pt", "ja", "ko", "zh", "yue"] {
        let r = Tier0Rules::new(Some(lang));
        let out = r.process("uh alpha beta", None);
        assert_eq!(out[0].text(), "Alpha beta", "'uh' survived in {lang}");
    }
}

#[test]
fn multi_word_discourse_markers_are_removed_at_the_opening() {
    assert_eq!(
        plain("you know the accessibility permission cannot be granted"),
        "The accessibility permission cannot be granted"
    );
    assert_eq!(
        plain("i mean the seam pass rejoins segments"),
        "The seam pass rejoins segments"
    );
}

#[test]
fn a_discourse_marker_used_literally_mid_sentence_survives() {
    // "you know" here is the verb, not a filler. Position is the only safe
    // discriminator without parsing, so mid-sentence occurrences are kept.
    assert_eq!(
        plain("i think you know the answer"),
        "I think you know the answer"
    );
}

#[test]
fn french_fillers_come_from_the_french_table() {
    let fr = Tier0Rules::new(Some("fr-FR"));
    let out = fr.process("euh nous avons livré", None);
    assert_eq!(out[0].text(), "Nous avons livré");
}

#[test]
fn portuguese_never_loses_um_because_it_is_a_real_word() {
    let pt = Tier0Rules::new(Some("pt-BR"));
    let out = pt.process("um carro rapido", None);
    assert_eq!(out[0].text(), "Um carro rapido");
}

#[test]
fn every_language_the_engine_supports_has_a_table() {
    // Apple SpeechTranscriber reports 30 locales across these ten languages.
    // A missing table is not unsafe -- it fails closed -- but it silently means
    // no cleanup at all, so the gap should be visible here rather than in use.
    for lang in ["en", "de", "es", "fr", "it", "pt", "ja", "ko", "zh", "yue"] {
        let r = Tier0Rules::new(Some(lang));
        assert_eq!(
            r.language(),
            lang,
            "no table for {lang}, fell back to {}",
            r.language()
        );
    }
}

#[test]
fn hedges_and_back_references_are_left_for_tier_1() {
    // "sort of working" is not "working", and "like I said" tells the listener
    // the speaker is repeating themselves. Tier 0 declines both.
    assert_eq!(plain("sort of works for now"), "Sort of works for now");
    assert_eq!(plain("like i said it works"), "Like i said it works");
}

// --- Segment edges, for the seam pass ---

#[test]
fn a_command_that_closes_a_segment_is_reported() {
    let seg = en().segment("we shipped it new paragraph", None);
    assert_eq!(seg.blocks.len(), 1);
    assert_eq!(seg.trailing, Some(Opening::Paragraph));
    assert_eq!(seg.opened_by, None);
}

#[test]
fn a_command_that_opens_a_segment_is_reported() {
    let seg = en().segment("numbered list review the spec", None);
    assert_eq!(seg.opened_by, Some(Opening::List { ordered: true }));
    assert_eq!(seg.trailing, None);
}

#[test]
fn a_segment_that_is_only_a_command_has_no_blocks_but_both_edges() {
    let seg = en().segment("new paragraph", None);
    assert!(seg.blocks.is_empty());
    assert_eq!(seg.opened_by, Some(Opening::Paragraph));
    assert_eq!(seg.trailing, Some(Opening::Paragraph));
}

#[test]
fn a_word_after_a_command_means_the_segment_did_not_end_with_it() {
    let seg = en().segment("new paragraph the tests pass", None);
    assert_eq!(seg.opened_by, Some(Opening::Paragraph));
    assert_eq!(seg.trailing, None);
}

#[test]
fn a_punctuating_engine_glues_punctuation_onto_commands() {
    // Heard in real use: Apple writes the spoken command as a sentence.
    let blocks = en().process("We shipped it. New paragraph. The tests pass.", None);
    assert_eq!(blocks.len(), 2, "got {blocks:?}");
    assert_eq!(blocks[0].text(), "We shipped it.");
    assert_eq!(blocks[1].text(), "The tests pass.");
}

#[test]
fn a_command_closes_the_block_before_it() {
    let blocks = en().process("two things numbered list review the spec", None);
    assert!(
        matches!(blocks[0], Block::Paragraph { .. }),
        "got {blocks:?}"
    );
    assert!(
        matches!(blocks[1], Block::ListItem { ordered: true, .. }),
        "got {blocks:?}"
    );
}

#[test]
fn bullet_is_a_word_outside_a_list_and_a_command_inside_one() {
    assert_eq!(plain("I like bullet points"), "I like bullet points");
    let blocks = en().process("new list apples bullet pears", None);
    assert_eq!(blocks.len(), 2, "got {blocks:?}");
    assert_eq!(blocks[1].text(), "Pears");
}

#[test]
fn a_filler_with_the_engines_comma_still_goes() {
    assert_eq!(plain("Um, the tests pass."), "The tests pass.");
    assert_eq!(plain("Okay, so this looks fine."), "This looks fine.");
}

#[test]
fn a_removed_filler_hands_its_capital_to_the_next_word() {
    let t0 = en().trusting_engine_case(true);
    let blocks = t0.process("Um, the tests pass.", None);
    assert_eq!(blocks[0].text(), "The tests pass.");
}

#[test]
fn a_trusted_engine_keeps_a_lowercase_opening() {
    // The engine heard a sentence continue across the boundary; the seam
    // pass needs to see that, so Tier 0 must not paper over it.
    let t0 = en().trusting_engine_case(true);
    assert_eq!(
        t0.process("courtesy of the seam.", None)[0].text(),
        "courtesy of the seam."
    );
    // But a block the user opened with a command starts with a capital.
    let blocks = t0.process("done. new paragraph the tests pass.", None);
    assert_eq!(blocks[1].text(), "The tests pass.");
}

#[test]
fn an_initial_filler_is_a_word_when_the_segment_continues_a_sentence() {
    let seg = en().segment_continuing("so we fixed it", true);
    assert_eq!(seg.blocks[0].text(), "So we fixed it");
    let seg = en().segment_continuing("so we fixed it", false);
    assert_eq!(seg.blocks[0].text(), "We fixed it");
}

#[test]
fn bare_heading_and_title_are_words_once_a_block_is_under_way() {
    let blocks = en().process("we are heading home", None);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(matches!(blocks[0], Block::Paragraph { .. }));
    assert_eq!(blocks[0].text(), "We are heading home");
    let blocks = en().process("the title of the book is long", None);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0].text(), "The title of the book is long");
}

#[test]
fn the_two_word_forms_open_a_heading_anywhere() {
    let blocks = en().process("some notes new heading resource budgets", None);
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(matches!(blocks[1], Block::Heading { .. }));
    assert_eq!(blocks[1].text(), "Resource budgets");
    let blocks = en().process("new title resource budgets", None);
    assert!(matches!(blocks[0], Block::Heading { .. }));
}

#[test]
fn a_segment_that_continues_a_sentence_keeps_heading_as_a_word() {
    // "...and then we are" [pause] "heading home": the second segment opens
    // with the word, not the command.
    let segment = en().segment_continuing("heading home", true);
    assert_eq!(segment.blocks.len(), 1, "{segment:?}");
    assert!(matches!(segment.blocks[0], Block::Paragraph { .. }));
    // A fresh segment that starts with it is still the command.
    let segment = en().segment_continuing("heading resource budgets", false);
    assert!(matches!(segment.blocks[0], Block::Heading { .. }));
}
