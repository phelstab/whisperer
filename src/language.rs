//! The languages whisper-large-v3 knows (its 100 language tokens, in whisper's own order) and the
//! short list offered in the tray menu.
//!
//! With "auto", whisper detects the language once from the first 30 seconds and transcribes the
//! whole recording in it; a fixed language skips detection, which is more reliable when the start
//! is silent or in another language. A recording that switches languages comes out in one of them
//! (whisper tends to translate the other parts).

/// (whisper code, English name). Note whisper's own codes: "jw" for Javanese, "yue" for Cantonese (new in v3).
pub const ALL: [(&str, &str); 100] = [
    ("en", "English"), ("zh", "Chinese"), ("de", "German"), ("es", "Spanish"), ("ru", "Russian"),
    ("ko", "Korean"), ("fr", "French"), ("ja", "Japanese"), ("pt", "Portuguese"), ("tr", "Turkish"),
    ("pl", "Polish"), ("ca", "Catalan"), ("nl", "Dutch"), ("ar", "Arabic"), ("sv", "Swedish"),
    ("it", "Italian"), ("id", "Indonesian"), ("hi", "Hindi"), ("fi", "Finnish"), ("vi", "Vietnamese"),
    ("he", "Hebrew"), ("uk", "Ukrainian"), ("el", "Greek"), ("ms", "Malay"), ("cs", "Czech"),
    ("ro", "Romanian"), ("da", "Danish"), ("hu", "Hungarian"), ("ta", "Tamil"), ("no", "Norwegian"),
    ("th", "Thai"), ("ur", "Urdu"), ("hr", "Croatian"), ("bg", "Bulgarian"), ("lt", "Lithuanian"),
    ("la", "Latin"), ("mi", "Maori"), ("ml", "Malayalam"), ("cy", "Welsh"), ("sk", "Slovak"),
    ("te", "Telugu"), ("fa", "Persian"), ("lv", "Latvian"), ("bn", "Bengali"), ("sr", "Serbian"),
    ("az", "Azerbaijani"), ("sl", "Slovenian"), ("kn", "Kannada"), ("et", "Estonian"), ("mk", "Macedonian"),
    ("br", "Breton"), ("eu", "Basque"), ("is", "Icelandic"), ("hy", "Armenian"), ("ne", "Nepali"),
    ("mn", "Mongolian"), ("bs", "Bosnian"), ("kk", "Kazakh"), ("sq", "Albanian"), ("sw", "Swahili"),
    ("gl", "Galician"), ("mr", "Marathi"), ("pa", "Punjabi"), ("si", "Sinhala"), ("km", "Khmer"),
    ("sn", "Shona"), ("yo", "Yoruba"), ("so", "Somali"), ("af", "Afrikaans"), ("oc", "Occitan"),
    ("ka", "Georgian"), ("be", "Belarusian"), ("tg", "Tajik"), ("sd", "Sindhi"), ("gu", "Gujarati"),
    ("am", "Amharic"), ("yi", "Yiddish"), ("lo", "Lao"), ("uz", "Uzbek"), ("fo", "Faroese"),
    ("ht", "Haitian Creole"), ("ps", "Pashto"), ("tk", "Turkmen"), ("nn", "Nynorsk"), ("mt", "Maltese"),
    ("sa", "Sanskrit"), ("lb", "Luxembourgish"), ("my", "Myanmar"), ("bo", "Tibetan"), ("tl", "Tagalog"),
    ("mg", "Malagasy"), ("as", "Assamese"), ("tt", "Tatar"), ("haw", "Hawaiian"), ("ln", "Lingala"),
    ("ha", "Hausa"), ("ba", "Bashkir"), ("jw", "Javanese"), ("su", "Sundanese"), ("yue", "Cantonese"),
];

/// Offered in the tray menu besides "Auto"; any other code can be set in the settings file.
pub const MENU: [&str; 12] = ["en", "de", "fr", "es", "it", "nl", "pt", "pl", "tr", "ru", "zh", "ja"];

/// Normalizes a setting ("DE", " en ", "de-DE", "auto") to a code whisper accepts, or `None` if
/// it's unknown. Regional tags are reduced to the language: whisper has no regional variants.
pub fn parse(setting: &str) -> Option<String> {
    let code = setting.trim().split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    (code == "auto" || ALL.iter().any(|(c, _)| *c == code)).then_some(code)
}

/// "German (de)", "Auto-detect", or the code itself if unknown.
pub fn describe(code: &str) -> String {
    match ALL.iter().find(|(c, _)| *c == code) {
        Some((c, name)) => format!("{name} ({c})"),
        None if code == "auto" => "Auto-detect".into(),
        None => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_menu_is_valid() {
        let mut codes: Vec<&str> = ALL.iter().map(|(c, _)| *c).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 100);
        assert!(MENU.iter().all(|m| parse(m).is_some()));
    }

    #[test]
    fn parses_settings() {
        assert_eq!(parse(" DE ").as_deref(), Some("de"));
        assert_eq!(parse("auto").as_deref(), Some("auto"));
        assert_eq!(parse("yue").as_deref(), Some("yue"));
        assert_eq!(parse("german"), None);
        assert_eq!(parse("de-DE").as_deref(), Some("de"));
        assert_eq!(parse("en_US").as_deref(), Some("en"));
        assert_eq!(describe("de"), "German (de)");
    }
}
