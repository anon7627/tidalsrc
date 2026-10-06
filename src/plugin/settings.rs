//! Settings the plugin declares to the host (`initialize` → `settings`),
//! and the values the host sends back (`initialize` params, then
//! `settings.changed`).

use serde_json::{json, Value};

use super::resolve::Quality;

/// How `auth.begin` signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    /// Code confirmed on `link.tidal.com`: nothing to paste, CD quality at
    /// most.
    Device,
    /// TIDAL's sign-in page, then the address it lands on pasted back:
    /// hi-res.
    Browser,
}

/// Values in effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Best quality asked for, whatever the output accepts.
    pub max_quality: Quality,
    pub sign_in: SignIn,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { max_quality: Quality::HiRes, sign_in: SignIn::Device }
    }
}

impl Settings {
    /// Values from the host's `{key: value}` map. Unknown keys and values of
    /// the wrong type are ignored; missing keys keep their default.
    pub fn from_values(values: Option<&Value>) -> Self {
        let mut s = Settings::default();
        let Some(values) = values.and_then(Value::as_object) else { return s };
        match values.get("quality").and_then(Value::as_str) {
            Some("hires") => s.max_quality = Quality::HiRes,
            Some("lossless") => s.max_quality = Quality::Lossless,
            Some("high") => s.max_quality = Quality::High,
            _ => {}
        }
        match values.get("sign_in").and_then(Value::as_str) {
            Some("device") => s.sign_in = SignIn::Device,
            Some("browser") => s.sign_in = SignIn::Browser,
            _ => {}
        }
        s
    }
}

/// The declaration, labelled in `lang` (French, else English).
pub fn declaration(lang: &str) -> Value {
    let fr = lang == "fr";
    let t = |en: &'static str, fr_text: &'static str| if fr { fr_text } else { en };
    json!([
        {
            "key": "quality", "type": "choice", "default": "hires",
            "section": t("Playback", "Lecture"),
            "label": t("Best quality", "Qualité maximale"),
            "description": t(
                "The best TIDAL quality asked for. Within it, each track comes in the best format your \
                 DAC plays natively. Applies from the next track.",
                "La meilleure qualité TIDAL demandée. Dans cette limite, chaque titre arrive dans le \
                 meilleur format que votre DAC lit nativement. S'applique à partir du titre suivant."
            ),
            "options": [
                {"value": "hires", "label": t("Hi-res (up to 24-bit/192 kHz FLAC)", "Haute résolution (FLAC jusqu'à 24 bits/192 kHz)")},
                {"value": "lossless", "label": t("CD quality (16-bit/44.1 kHz FLAC)", "Qualité CD (FLAC 16 bits/44,1 kHz)")},
                {"value": "high", "label": t("High (AAC 320 kbit/s)", "Élevée (AAC 320 kbit/s)")},
            ],
        },
        {
            "key": "sign_in", "type": "choice", "default": "device",
            "section": t("Account", "Compte"),
            "label": t("Sign-in method", "Méthode de connexion"),
            "description": t(
                "With a code: confirm a code on link.tidal.com, nothing to paste, but TIDAL then streams \
                 CD quality at most. With the TIDAL page: sign in on TIDAL's page, then paste the address \
                 it lands on (it contains \"code=\"); hi-res is available. Takes effect at the next sign-in.",
                "Avec un code : confirmez un code sur link.tidal.com, rien à coller, mais TIDAL ne diffuse \
                 alors qu'en qualité CD au mieux. Avec la page TIDAL : connectez-vous sur la page de TIDAL, \
                 puis collez l'adresse où elle aboutit (elle contient « code= ») ; la haute résolution est \
                 disponible. S'applique à la prochaine connexion."
            ),
            "options": [
                {"value": "device", "label": t("With a code (link.tidal.com)", "Avec un code (link.tidal.com)")},
                {"value": "browser", "label": t("With the TIDAL page (hi-res)", "Avec la page TIDAL (haute résolution)")},
            ],
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_fall_back_to_defaults() {
        assert_eq!(Settings::from_values(None), Settings::default());
        let s = Settings::from_values(Some(&json!({"quality": "lossless", "sign_in": "browser", "gone": 3})));
        assert_eq!(s, Settings { max_quality: Quality::Lossless, sign_in: SignIn::Browser });
        let s = Settings::from_values(Some(&json!({"quality": 7, "sign_in": "carrier pigeon"})));
        assert_eq!(s, Settings::default(), "values of the wrong type or unknown are ignored");
    }

    #[test]
    fn declaration_is_translated_and_complete() {
        let fr = declaration("fr");
        let en = declaration("de");
        assert_eq!(fr[0]["label"], "Qualité maximale");
        assert_eq!(en[0]["label"], "Best quality");
        for entry in fr.as_array().unwrap() {
            assert_eq!(entry["type"], "choice");
            let options = entry["options"].as_array().unwrap();
            assert!(options.iter().any(|o| o["value"] == entry["default"]), "default among the options");
            assert!(entry.get("restart").is_none(), "applied without restarting the plugin");
        }
    }
}
