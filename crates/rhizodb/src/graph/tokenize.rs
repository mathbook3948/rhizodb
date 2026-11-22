use unicode_segmentation::UnicodeSegmentation;

/// What the terms are for. A document also keeps single characters of a paired-script run, so a
/// one-character query finds it, while a query of a longer run looks only for its pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Document,
    Query,
}

/// Splits `text` into lowercase terms at Unicode word boundaries, dropping control characters,
/// so no term holds a zero byte. Hangul, Han, Hiragana, and
/// Katakana runs become overlapping character pairs, because those scripts attach particles to
/// words or leave out spaces. The same text always gives the same terms, in the same order.
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
