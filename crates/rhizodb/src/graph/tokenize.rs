use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

const VERSION: u32 = 3;

const PROBE: &str = concat!(
    "Don't stop at 3.14, e-mail_me or ŌΔΟΣ İstanbul STRAßE Ǆemal ᲛᲐᲠᲗ ᏣᎳᎩ Ꭰ. ",
    "हिन्दी ภาษาไทย עִבְרִית العَرَبِيَّة 👩\u{200d}💻 a\u{0}\u{903}b x\u{7}\u{301}y \u{feff}z ",
    "서울에서 만나요 \u{1100}\u{1161}\u{11a8} 東京都に行く カタカナ ｶﾀｶﾅ 北京\u{2a6df}\u{31350}\u{323af} ",
    "\u{1e290}\u{1e291} \u{11f04}\u{11f05} \u{16d43}\u{16d44} \u{10d50}\u{10d51} \u{1e5d0}\u{1e5d1} ",
    "\u{2ebf0}\u{2ee5d} \u{1e030}\u{1e031} \u{0870}\u{0897} \u{1d7ff}\u{1df25} \u{10efc}\u{11b00} ",
    "\u{0378}\u{0379} \u{05ff}\u{08b5} \u{1e7e0}\u{1e7e7} \u{2b73a}\u{31350} \u{3134b}\u{e0001} ",
    "Cafe\u{301} A\u{30a}\u{323} ﬁ Ｘ² ① ㎏ \u{1109}\u{1165} \u{0928}\u{093c} \u{2126} \u{fb2c} ",
    "\u{1d15f} \u{2adc} \u{11099}\u{110ba} \u{1133b}\u{1133e} \u{0f73} \u{1e9b}\u{323} ",
    "ก่อน ๒๕๖๗ a\u{e48} ພາສາລາວ ភាសាខ្មែរ မြန်မာဘာသာ \u{1a20}\u{1a60}\u{1a21} \u{aa80}\u{aab0} ",
    "\u{19d1}\u{1950}\u{1951} \u{11700}\u{1171d}\u{116d0} \u{a9e0}\u{aa60}",
);

/// The tokenizer's version and its terms for a fixed probe text. The probe exercises the Unicode
/// tables the tokenizer depends on, including recently assigned and still unassigned code points,
/// so a change to those tables that touches the probe changes the fingerprint even when the
/// version does not.
pub(crate) fn fingerprint() -> Vec<u8> {
    let mut out = VERSION.to_be_bytes().to_vec();
    for purpose in [Purpose::Document, Purpose::Query] {
        for term in terms(PROBE, purpose) {
            out.extend_from_slice(term.as_bytes());
            out.push(0);
        }
        out.push(0);
    }
    out
}

/// What the terms are for. A document also keeps the single grapheme clusters of a paired-script
/// run, so a one-cluster query finds it, while a query of a longer run looks only for its pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Document,
    Query,
}

/// Splits `text` into lowercase terms at Unicode word boundaries, dropping control characters,
/// so no term holds a zero byte. Text and terms are in Unicode normalization form NFKC, so
/// canonically equivalent text gives the same terms, and so do compatibility forms such as
/// ligatures and full-width letters. Runs of Hangul, Han, Hiragana, Katakana, and the scripts
/// that Unicode line breaking leaves to a dictionary, such as Thai, become overlapping pairs of
/// grapheme clusters, because those scripts attach particles to words or leave out spaces
/// between words. A run starts at a letter that is not a digit. The same text always gives the
/// same terms, in the same order.
pub(crate) fn terms(text: &str, purpose: Purpose) -> Vec<String> {
    let mut terms = Vec::new();
    let mut run = String::new();
    let text: String = text.nfkc().collect();
    for segment in text.split_word_bounds() {
        if !segment.chars().any(char::is_alphanumeric) {
            flush_run(&mut run, purpose, &mut terms);
            continue;
        }
        let mut word = String::new();
        for character in segment.chars().filter(|character| !character.is_control()) {
            if joins_run(character, &run) {
                push_word(&mut word, &mut terms);
                run.push(character);
            } else {
                flush_run(&mut run, purpose, &mut terms);
                word.push(character);
            }
        }
        push_word(&mut word, &mut terms);
    }
    flush_run(&mut run, purpose, &mut terms);
    terms
}

fn push_word(word: &mut String, terms: &mut Vec<String>) {
    if word.chars().any(char::is_alphanumeric) {
        terms.push(word.to_lowercase().nfkc().collect());
    }
    word.clear();
}

fn flush_run(run: &mut String, purpose: Purpose, terms: &mut Vec<String>) {
    let units: Vec<&str> = run.graphemes(true).collect();
    if let [single] = units.as_slice() {
        terms.push((*single).to_owned());
    } else {
        terms.extend(units.windows(2).map(|pair| pair.concat()));
        if purpose == Purpose::Document {
            terms.extend(units.iter().map(|unit| (*unit).to_owned()));
        }
    }
    run.clear();
}

fn joins_run(character: char, run: &str) -> bool {
    is_paired_script(character)
        && !character.is_numeric()
        && (character.is_alphabetic() || !run.is_empty())
}

fn is_paired_script(character: char) -> bool {
    matches!(
        u32::from(character),
        0x0E00..=0x0EFF
            | 0x1000..=0x109F
            | 0x1100..=0x11FF
            | 0x1780..=0x17FF
            | 0x1950..=0x19FF
            | 0x1A20..=0x1AAF
            | 0x2E80..=0x2FDF
            | 0x3040..=0x30FF
            | 0x3130..=0x318F
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xA9E0..=0xA9FF
            | 0xAA60..=0xAADF
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFFDC
            | 0x116D0..=0x116FF
            | 0x11700..=0x1174F
            | 0x20000..=0x323AF
    )
}

#[cfg(test)]
#[path = "tokenize.test.rs"]
mod tests;
