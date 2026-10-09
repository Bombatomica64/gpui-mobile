//! Maps GPUI's [`TextInputConfiguration`] onto the `EditorInfo` a host's IME proxy
//! hands the keyboard: `inputType` and `imeOptions`.

use gpui::{Autocapitalize, TextInputAction, TextInputConfiguration};

use crate::KeyboardType;

// android.text.InputType
const TYPE_MASK_CLASS: i32 = 0xf;
const TYPE_CLASS_TEXT: i32 = 0x1;
const TYPE_CLASS_NUMBER: i32 = 0x2;
const TYPE_CLASS_PHONE: i32 = 0x3;
const TYPE_TEXT_VARIATION_URI: i32 = 0x10;
const TYPE_TEXT_VARIATION_EMAIL_ADDRESS: i32 = 0x20;
const TYPE_NUMBER_FLAG_DECIMAL: i32 = 0x2000;
const TYPE_TEXT_FLAG_CAP_CHARACTERS: i32 = 0x1000;
const TYPE_TEXT_FLAG_CAP_WORDS: i32 = 0x2000;
const TYPE_TEXT_FLAG_CAP_SENTENCES: i32 = 0x4000;
const TYPE_TEXT_FLAG_AUTO_CORRECT: i32 = 0x8000;
const TYPE_TEXT_FLAG_MULTI_LINE: i32 = 0x20000;
const TYPE_TEXT_FLAG_NO_SUGGESTIONS: i32 = 0x80000;

// android.view.inputmethod.EditorInfo
const IME_ACTION_UNSPECIFIED: i32 = 0;
const IME_ACTION_GO: i32 = 2;
const IME_ACTION_SEARCH: i32 = 3;
const IME_ACTION_SEND: i32 = 4;
const IME_ACTION_NEXT: i32 = 5;
const IME_ACTION_DONE: i32 = 6;
const IME_ACTION_PREVIOUS: i32 = 7;
/// GPUI draws the field, so the IME must never cover the app with its own.
const IME_FLAG_NO_EXTRACT_UI: i32 = 0x1000_0000;

/// `(inputType, imeOptions)` for a field with this configuration.
///
/// `keyboard` is the type an app asked for through
/// [`crate::show_keyboard_with_type`]; anything but the default picks the class.
///
/// A field whose confirm key inserts a line break (`Enter`, or no hint at all)
/// is multi-line; any other action makes it a single-line field showing that
/// action, which the host reports back as IME event 6.
pub(super) fn editor_info(
    configuration: &TextInputConfiguration,
    keyboard: KeyboardType,
) -> (i32, i32) {
    let mut input_type = match keyboard {
        KeyboardType::Default => TYPE_CLASS_TEXT,
        KeyboardType::EmailAddress => TYPE_CLASS_TEXT | TYPE_TEXT_VARIATION_EMAIL_ADDRESS,
        KeyboardType::Phone => TYPE_CLASS_PHONE,
        KeyboardType::NumberPad => TYPE_CLASS_NUMBER,
        KeyboardType::URL => TYPE_CLASS_TEXT | TYPE_TEXT_VARIATION_URI,
        KeyboardType::Decimal => TYPE_CLASS_NUMBER | TYPE_NUMBER_FLAG_DECIMAL,
    };
    let action = match configuration.input_action {
        TextInputAction::Unspecified | TextInputAction::Enter => IME_ACTION_UNSPECIFIED,
        TextInputAction::Done => IME_ACTION_DONE,
        TextInputAction::Go => IME_ACTION_GO,
        TextInputAction::Next => IME_ACTION_NEXT,
        TextInputAction::Previous => IME_ACTION_PREVIOUS,
        TextInputAction::Search => IME_ACTION_SEARCH,
        TextInputAction::Send => IME_ACTION_SEND,
    };
    if input_type & TYPE_MASK_CLASS == TYPE_CLASS_TEXT {
        if action == IME_ACTION_UNSPECIFIED {
            input_type |= TYPE_TEXT_FLAG_MULTI_LINE;
        }
        input_type |= match configuration.autocapitalize {
            Autocapitalize::None => 0,
            Autocapitalize::Words => TYPE_TEXT_FLAG_CAP_WORDS,
            Autocapitalize::Sentences => TYPE_TEXT_FLAG_CAP_SENTENCES,
            Autocapitalize::Characters => TYPE_TEXT_FLAG_CAP_CHARACTERS,
        };
        if configuration.autocorrect {
            input_type |= TYPE_TEXT_FLAG_AUTO_CORRECT;
        }
        if !configuration.suggestions {
            input_type |= TYPE_TEXT_FLAG_NO_SUGGESTIONS;
        }
    }
    (input_type, action | IME_FLAG_NO_EXTRACT_UI)
}

/// Whether the IME action `action` (an `IME_ACTION_*`) ends editing, so the
/// keyboard goes away after the field has seen its enter key. Next and Previous
/// move between fields and keep it.
pub(super) fn action_dismisses_keyboard(action: i32) -> bool {
    matches!(
        action,
        IME_ACTION_DONE | IME_ACTION_GO | IME_ACTION_SEARCH | IME_ACTION_SEND
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_assistance_maps_to_flags() {
        let configuration = TextInputConfiguration {
            autocorrect: true,
            autocapitalize: Autocapitalize::Sentences,
            suggestions: true,
            input_action: TextInputAction::Enter,
        };
        assert_eq!(
            editor_info(&configuration, KeyboardType::Default),
            (
                TYPE_CLASS_TEXT
                    | TYPE_TEXT_FLAG_MULTI_LINE
                    | TYPE_TEXT_FLAG_CAP_SENTENCES
                    | TYPE_TEXT_FLAG_AUTO_CORRECT,
                IME_ACTION_UNSPECIFIED | IME_FLAG_NO_EXTRACT_UI
            )
        );
    }

    #[test]
    fn an_action_makes_a_single_line_field() {
        let configuration = TextInputConfiguration {
            suggestions: true,
            input_action: TextInputAction::Search,
            ..Default::default()
        };
        assert_eq!(
            editor_info(&configuration, KeyboardType::Default),
            (TYPE_CLASS_TEXT, IME_ACTION_SEARCH | IME_FLAG_NO_EXTRACT_UI)
        );
    }

    #[test]
    fn the_default_configuration_disables_assistance() {
        let (input_type, _) =
            editor_info(&TextInputConfiguration::default(), KeyboardType::Default);
        assert_eq!(
            input_type,
            TYPE_CLASS_TEXT | TYPE_TEXT_FLAG_MULTI_LINE | TYPE_TEXT_FLAG_NO_SUGGESTIONS
        );
    }

    #[test]
    fn text_flags_stay_off_numeric_keyboards() {
        let configuration = TextInputConfiguration {
            autocorrect: true,
            ..Default::default()
        };
        let (input_type, _) = editor_info(&configuration, KeyboardType::Decimal);
        assert_eq!(input_type, TYPE_CLASS_NUMBER | TYPE_NUMBER_FLAG_DECIMAL);
    }

    #[test]
    fn only_finishing_actions_dismiss_the_keyboard() {
        assert!(action_dismisses_keyboard(IME_ACTION_DONE));
        assert!(action_dismisses_keyboard(IME_ACTION_SEND));
        assert!(!action_dismisses_keyboard(IME_ACTION_NEXT));
        assert!(!action_dismisses_keyboard(IME_ACTION_UNSPECIFIED));
    }
}
