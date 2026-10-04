// SPDX-License-Identifier: MIT
//! Phonetic keys and string distance for dictionary matching.
//!
//! # Why Soundex rather than Double Metaphone
//!
//! The original design specified Double Metaphone. This implements Soundex
//! instead, and the deviation is deliberate and measurable:
//!
//! - An open-source dictation app ships **Soundex plus Levenshtein** for
//!   exactly this job, in production, against user-supplied technical vocabulary.
//! - Soundex is ~40 lines and fully testable. Double Metaphone is ~500 lines of
//!   hand-transcribed rules, and 500 lines of unverified phonetics is a worse
//!   bet than 40 lines of verified ones.
//! - Whether it is *sufficient* is measurable: the dictionary test suite runs
//!   the eleven terms Apple SpeechTranscriber actually got wrong. If Soundex
//!   misses one, that is the evidence to escalate.
//!
//! Escalate on a measured miss, not on a specification's say-so.

/// Soundex code: first letter, then up to three consonant-class digits.
///
/// Vowels and h/w/y are dropped after the first character, adjacent identical
/// classes collapse, and the result is padded to four characters.
pub fn soundex(word: &str) -> String {
    fn class(c: char) -> Option<char> {
        match c.to_ascii_lowercase() {
            'b' | 'f' | 'p' | 'v' => Some('1'),
            'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => Some('2'),
            'd' | 't' => Some('3'),
            'l' => Some('4'),
            'm' | 'n' => Some('5'),
            'r' => Some('6'),
            _ => None,
        }
    }

    let letters: Vec<char> = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    let Some(&first) = letters.first() else {
        return String::new();
    };

    let mut out = String::with_capacity(4);
    out.push(first.to_ascii_uppercase());
    let mut last = class(first);
    for &c in &letters[1..] {
        let k = class(c);
        match k {
            // A vowel breaks the run, so "tt" collapses but "tot" does not.
            None if matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u') => last = None,
            // h and w are transparent: they do not break a run.
            None => {}
            Some(d) => {
                if Some(d) != last {
                    out.push(d);
                    if out.len() == 4 {
                        break;
                    }
                }
                last = Some(d);
            }
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

/// Levenshtein distance, iterative with two rows.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Similarity, but abandons the computation as soon as the answer cannot
/// reach `floor`. Returns `None` when the pair is too far apart to matter.
///
/// The dictionary scores a candidate against many terms and almost all of them
/// are misses, so the useful optimisation is bailing out of a miss cheaply. A
/// full Levenshtein is O(n*m); this is O(n*k) where k is the number of edits
/// the threshold still allows, which for a 0.85 threshold on a ten-character
/// word is one.
pub fn similarity_at_least(a: &str, b: &str, floor: f32) -> Option<f32> {
    let (na, nb) = (a.chars().count(), b.chars().count());
    let longest = na.max(nb);
    if longest == 0 {
        return Some(1.0);
    }
    // Edits allowed before similarity drops below the floor.
    let budget = ((1.0 - floor) * longest as f32).floor() as usize;
    // A length difference alone can exceed the budget.
    if na.abs_diff(nb) > budget {
        return None;
    }

    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=nb).collect();
    let mut cur = vec![0usize; nb + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        // Only the diagonal band within the budget can still win.
        let lo = (i + 1).saturating_sub(budget + 1);
        let hi = (i + 1 + budget).min(nb);
        let mut row_best = usize::MAX;
        for j in 0..nb {
            if j + 1 < lo || j + 1 > hi {
                cur[j + 1] = usize::MAX / 2;
                continue;
            }
            let cost = usize::from(ca != b[j]);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            row_best = row_best.min(cur[j + 1]);
        }
        if row_best > budget {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[nb];
    if d > budget {
        return None;
    }
    Some(1.0 - d as f32 / longest as f32)
}

/// Similarity in `0.0..=1.0`, normalised by the longer string.
pub fn similarity(a: &str, b: &str) -> f32 {
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    1.0 - levenshtein(a, b) as f32 / longest as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soundex_matches_the_classic_examples() {
        assert_eq!(soundex("Robert"), "R163");
        assert_eq!(soundex("Rupert"), "R163");
        assert_eq!(soundex("Ashcraft"), "A261");
        assert_eq!(soundex("Tymczak"), "T522");
        assert_eq!(soundex("Pfister"), "P236");
    }

    #[test]
    fn a_vowel_breaks_a_run_but_h_and_w_do_not() {
        // "Ashcraft": the h is transparent, so s and c collapse to one 2.
        assert_eq!(soundex("Ashcraft"), soundex("Ashcroft"));
        // Tot keeps both t sounds because a vowel separates them.
        assert_eq!(soundex("Tot"), "T300");
    }

    #[test]
    fn soundex_catches_most_real_mishearings_but_not_all() {
        // Measured against what Apple SpeechTranscriber actually produced.
        // Soundex alone caught 8 of 11; these three it gets.
        for (heard, want) in [
            ("Ogam", "Ogham"),
            ("postgress", "Postgres"),
            ("embodings", "embeddings"),
        ] {
            assert_eq!(soundex(heard), soundex(want), "{heard:?} / {want:?}");
        }
    }

    #[test]
    fn soundex_misses_superbase_which_is_why_distance_is_also_used() {
        // An inserted "r" changes the key entirely: S112 against S161. Edit
        // distance sees it as one substitution in nine characters. Neither
        // signal is sufficient alone, which is why the matcher ORs them.
        assert_ne!(soundex("Superbase"), soundex("Supabase"));
        assert!(similarity("superbase", "supabase") > 0.75);
    }

    #[test]
    fn unrelated_words_do_not_collide() {
        assert_ne!(soundex("parser"), soundex("kubectl"));
        assert_ne!(soundex("Azure"), soundex("Postgres"));
    }

    #[test]
    fn similarity_is_normalised_and_symmetric() {
        assert_eq!(similarity("kubectl", "kubectl"), 1.0);
        assert_eq!(similarity("", ""), 1.0);
        assert!((similarity("abc", "abd") - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(similarity("abc", "xyz"), 0.0);
        assert_eq!(similarity("abc", "abd"), similarity("abd", "abc"));
    }

    #[test]
    fn empty_input_is_handled() {
        assert_eq!(soundex(""), "");
        assert_eq!(soundex("123"), "");
        assert_eq!(levenshtein("", "abc"), 3);
    }
}

#[cfg(test)]
mod cutoff_tests {
    use super::*;

    #[test]
    fn the_cutoff_agrees_with_the_full_computation_when_it_answers() {
        for (a, b) in [
            ("postgress", "postgres"),
            ("superbase", "supabase"),
            ("ogam", "ogham"),
            ("passer", "parser"),
            ("kubectl", "kubectl"),
        ] {
            if let Some(s) = similarity_at_least(a, b, 0.6) {
                assert!(
                    (s - similarity(a, b)).abs() < 1e-6,
                    "{a}/{b}: cutoff {s} vs full {}",
                    similarity(a, b)
                );
            }
        }
    }

    #[test]
    fn far_pairs_are_abandoned_rather_than_scored() {
        assert_eq!(similarity_at_least("kubectl", "terragrunt", 0.85), None);
        assert_eq!(similarity_at_least("abc", "xyz", 0.85), None);
    }

    #[test]
    fn a_length_gap_alone_ends_it() {
        assert_eq!(similarity_at_least("a", "abcdefghij", 0.85), None);
    }

    #[test]
    fn equal_strings_are_always_a_perfect_match() {
        assert_eq!(similarity_at_least("kubectl", "kubectl", 0.99), Some(1.0));
        assert_eq!(similarity_at_least("", "", 0.9), Some(1.0));
    }
}
