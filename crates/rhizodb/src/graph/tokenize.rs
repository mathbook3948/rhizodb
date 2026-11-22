use unicode_segmentation::UnicodeSegmentation;

const VERSION: u32 = 1;

const PROBE: &str = concat!(
    "Don't stop at 3.14, e-mail_me or ŌΔΟΣ İstanbul STRAßE Ǆemal ᲛᲐᲠᲗ ᏣᎳᎩ Ꭰ. ",
    "हिन्दी ภาษาไทย עִבְרִית العَرَبِيَّة 👩\u{200d}💻 a\u{0}\u{903}b x\u{7}\u{301}y \u{feff}z ",
    "서울에서 만나요 \u{1100}\u{1161}\u{11a8} 東京都に行く カタカナ ｶﾀｶﾅ 北京\u{2a6df}\u{31350}\u{323af} ",
    "\u{1e290}\u{1e291} \u{11f04}\u{11f05} \u{16d43}\u{16d44} \u{10d50}\u{10d51} \u{1e5d0}\u{1e5d1} ",
    "\u{2ebf0}\u{2ee5d} \u{1e030}\u{1e031} \u{0870}\u{0897} \u{1d7ff}\u{1df25} \u{10efc}\u{11b00} ",
    "\u{0378}\u{0379} \u{05ff}\u{08b5} \u{1e7e0}\u{1e7e7} \u{2b73a}\u{31350} \u{3134b}\u{e0001}",
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

/// What the terms are for. A document also keeps single characters of a paired-script run, so a
/// one-character query finds it, while a query of a longer run looks only for its pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Document,
    Query,
}

/// Splits `text` into lowercase terms at Unicode word boundaries, dropping control characters,
/// so no term holds a zero byte. Hangul, Han, Hiragana, and Katakana runs become overlapping
/// character pairs, because those scripts attach particles to words or leave out spaces. The
/// same text always gives the same terms, in the same order.
pub(crate) fn terms(text: &str, purpose: Purpose) -> Vec<String> {
    let mut terms = Vec::new();
    let mut run = Vec::new();
    for segment in text.split_word_bounds() {
        if !segment.chars().any(char::is_alphanumeric) {
            flush_run(&mut run, purpose, &mut terms);
            continue;
        }
        let mut word = String::new();
        for character in segment.chars().filter(|character| !character.is_control()) {
            if is_paired_script(character) {
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
        terms.push(word.to_lowercase());
    }
    word.clear();
}

fn flush_run(run: &mut Vec<char>, purpose: Purpose, terms: &mut Vec<String>) {
    if let [single] = run.as_slice() {
        terms.push(single.to_string());
    } else {
        terms.extend(run.windows(2).map(|pair| pair.iter().collect()));
        if purpose == Purpose::Document {
            terms.extend(run.iter().map(char::to_string));
        }
    }
    run.clear();
}

fn is_paired_script(character: char) -> bool {
    matches!(
        u32::from(character),
        0x1100..=0x11FF
            | 0x2E80..=0x2FDF
            | 0x3040..=0x30FF
            | 0x3130..=0x318F
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFFDC
            | 0x20000..=0x323AF
    )
}

#[cfg(test)]
#[path = "tokenize.test.rs"]
mod tests;
