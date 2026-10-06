// The optional "Hey Coucou" in front of a request.
//
// This is not wake-word detection. The microphone is already open, because the
// person held the shortcut; the phrase is looked for in the *text* that came back,
// and only at its very start. Nothing here ever hears anything.

/// The longest phrase settings accept.
pub const MAX_PHRASE_CHARS: usize = 40;

pub const DEFAULT_PHRASE: &str = "Hey Coucou";

/// What a phrase may contain: words, and the spaces, apostrophes and hyphens between.
/// It is only ever compared with text, never run or sent anywhere.
pub fn valid_phrase(phrase: &str) -> bool {
    let phrase = phrase.trim();
    !phrase.is_empty()
        && phrase.chars().count() <= MAX_PHRASE_CHARS
        && phrase.chars().any(char::is_alphanumeric)
        && phrase
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '\'' | '’' | '-' | ',' | '.' | '!'))
}

fn is_word_char(c: char) -> bool {
    // An apostrophe belongs to its word, so "Coucou's" is not "Coucou".
    c.is_alphanumeric() || c == '\'' || c == '’'
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// `transcript` without a leading `phrase`, tidied up and capitalised. A transcript
/// that does not *start* with the phrase comes back unchanged (apart from trimming),
/// so "I was talking about Coucou yesterday." is left alone.
pub fn strip_leading_phrase(transcript: &str, phrase: &str) -> String {
    let unchanged = || transcript.trim().to_string();
    let wanted = words(phrase);
    if wanted.is_empty() {
        return unchanged();
    }
    let mut rest = transcript;
    for word in &wanted {
        rest = rest.trim_start_matches(|c: char| !is_word_char(c));
        let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
        if rest[..end].to_lowercase() != *word {
            return unchanged();
        }
        rest = &rest[end..];
    }
    let rest = rest.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?' | ':' | ';' | '-' | '–' | '—' | '…')
    });
    capitalise(rest.trim_end())
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str) -> String {
        strip_leading_phrase(text, DEFAULT_PHRASE)
    }

    #[test]
    fn a_leading_phrase_is_removed_and_the_request_capitalised() {
        assert_eq!(strip("Hey Coucou, open Notepad."), "Open Notepad.");
        assert_eq!(
            strip("Hey Coucou, what's the weather?"),
            "What's the weather?"
        );
        assert_eq!(strip("Hey Coucou, find this file."), "Find this file.");
    }

    #[test]
    fn capitalisation_punctuation_and_spacing_are_forgiven() {
        for text in [
            "HEY COUCOU, open Notepad.",
            "hey coucou open notepad.",
            "Hey Coucou! Open Notepad.",
            "Hey, Coucou, open Notepad.",
            "  Hey   Coucou ... open Notepad.  ",
            "Hey Coucou: open Notepad.",
            "Hey Coucou - open Notepad.",
            "\"Hey Coucou, open Notepad.\"",
        ] {
            let got = strip(text);
            // A closing quote belongs to the request, and "notepad" may be lower case.
            assert!(
                got.to_lowercase().starts_with("open notepad."),
                "{text:?} -> {got:?}"
            );
            assert!(got.starts_with("Open"), "{text:?} -> {got:?}");
        }
        assert_eq!(strip("  Hey   Coucou ... open Notepad.  "), "Open Notepad.");
    }

    #[test]
    fn mentions_of_the_name_elsewhere_are_left_alone() {
        for text in [
            "I was talking about Coucou yesterday.",
            "Coucou, open Notepad.",
            "Open Notepad, hey Coucou.",
            "Say hey Coucou to Notepad",
            "Hey there Coucou, open Notepad.",
            "Heyyy Coucou open Notepad.",
            "Hey Coucouville, open Notepad.",
            "Hey Coucou's notes, please.",
            "Hey",
        ] {
            assert_eq!(strip(text), text.trim(), "{text:?}");
        }
    }

    #[test]
    fn the_phrase_alone_leaves_nothing_to_do() {
        assert_eq!(strip("Hey Coucou."), "");
        assert_eq!(strip("hey coucou"), "");
        assert_eq!(strip("Hey Coucou, ..."), "");
    }

    #[test]
    fn only_the_first_occurrence_is_removed() {
        assert_eq!(
            strip("Hey Coucou, tell hey Coucou hello."),
            "Tell hey Coucou hello."
        );
    }

    #[test]
    fn a_custom_phrase_works_and_an_empty_one_removes_nothing() {
        assert_eq!(
            strip_leading_phrase("OK Mochi, open Notepad.", "ok mochi"),
            "Open Notepad."
        );
        assert_eq!(
            strip_leading_phrase("Hey Coucou, open Notepad.", "ok mochi"),
            "Hey Coucou, open Notepad."
        );
        assert_eq!(strip_leading_phrase(" Open Notepad. ", ""), "Open Notepad.");
        assert_eq!(
            strip_leading_phrase(" Open Notepad. ", " , "),
            "Open Notepad."
        );
    }

    #[test]
    fn accents_and_other_scripts_compare_by_letter() {
        assert_eq!(
            strip_leading_phrase("Écoute Coucou, ouvre le Bloc-notes.", "écoute coucou"),
            "Ouvre le Bloc-notes."
        );
        assert_eq!(
            strip_leading_phrase("ÉCOUTE COUCOU ouvre", "écoute coucou"),
            "Ouvre"
        );
    }

    #[test]
    fn phrases_are_checked_before_they_are_saved() {
        assert!(valid_phrase("Hey Coucou"));
        assert!(valid_phrase("  Écoute, Coucou! "));
        assert!(valid_phrase("ok mochi's friend"));
        assert!(!valid_phrase(""));
        assert!(!valid_phrase("   "));
        assert!(!valid_phrase("!!!"));
        assert!(!valid_phrase("hey <script>"));
        assert!(!valid_phrase("hey\ncoucou"));
        assert!(!valid_phrase(&"a".repeat(MAX_PHRASE_CHARS + 1)));
        assert!(valid_phrase(&"a".repeat(MAX_PHRASE_CHARS)));
    }
}
