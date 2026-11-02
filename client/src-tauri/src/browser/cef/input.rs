//! The overlay's gestures, as the wire carries them.
//!
//! **A gesture, not the protocol.** The server turns a press and a wheel into
//! CEF's own mouse calls; this half names what the person did and the
//! modifier keys they held. The mapping is pure, so the shapes are tested
//! without a window.

use forge_primitives::browser::InputModifiers;

/// The wire's wheel deltas for an AppKit scroll event.
///
/// **AppKit's sign is the opposite of the wire's.** The wire carries the
/// page's sign, which the server turns back for CEF, while AppKit reports a
/// scroll up as a positive delta.
pub fn wheel_deltas(scrolling_delta_x: f64, scrolling_delta_y: f64) -> (f64, f64) {
    (-scrolling_delta_x, -scrolling_delta_y)
}

/// The modifier keys one AppKit event held.
pub fn modifiers(flags: objc2_app_kit::NSEventModifierFlags) -> InputModifiers {
    use objc2_app_kit::NSEventModifierFlags as Flags;
    InputModifiers {
        alt: flags.contains(Flags::Option),
        control: flags.contains(Flags::Control),
        meta: flags.contains(Flags::Command),
        shift: flags.contains(Flags::Shift),
    }
}

/// The page's specials, by macOS virtual keycode: what a text insert cannot
/// express and a widget reads as a key.
pub fn special_key(keycode: u16) -> Option<(&'static str, &'static str, i32)> {
    // (DOM key, DOM code, Windows virtual key)
    Some(match keycode {
        36 => ("Enter", "Enter", 13),
        48 => ("Tab", "Tab", 9),
        51 => ("Backspace", "Backspace", 8),
        53 => ("Escape", "Escape", 27),
        115 => ("Home", "Home", 36),
        116 => ("PageUp", "PageUp", 33),
        117 => ("Delete", "Delete", 46),
        119 => ("End", "End", 35),
        121 => ("PageDown", "PageDown", 34),
        123 => ("ArrowLeft", "ArrowLeft", 37),
        124 => ("ArrowRight", "ArrowRight", 39),
        125 => ("ArrowDown", "ArrowDown", 40),
        126 => ("ArrowUp", "ArrowUp", 38),
        _ => return None,
    })
}

/// A letter key with Command held: the browser's own edit shortcuts, which a
/// text insert cannot express.
pub fn editing_key(character: char) -> Option<(&'static str, &'static str, i32)> {
    Some(match character.to_ascii_lowercase() {
        'a' => ("a", "KeyA", 65),
        'x' => ("x", "KeyX", 88),
        'c' => ("c", "KeyC", 67),
        'v' => ("v", "KeyV", 86),
        'z' => ("z", "KeyZ", 90),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specials are the page's own names, not AppKit's keycodes: a
    /// rename here types a literal character instead of pressing a key.
    #[test]
    fn a_special_key_names_its_dom_key() {
        assert_eq!(special_key(36), Some(("Enter", "Enter", 13)));
        assert_eq!(special_key(123), Some(("ArrowLeft", "ArrowLeft", 37)));
        assert_eq!(special_key(0), None, "a letter is text, not a special");
    }

    /// Only the five editing letters are chords; anything else with Command
    /// is the app's menu or plain typing.
    #[test]
    fn only_the_editing_letters_are_chords() {
        assert_eq!(editing_key('a'), Some(("a", "KeyA", 65)));
        assert_eq!(editing_key('V'), Some(("v", "KeyV", 86)));
        assert_eq!(editing_key('b'), None);
    }

    /// **A native wheel crosses in the page's sign, both ways.** AppKit's
    /// positive delta is a scroll up, and the wire's positive one a scroll
    /// down; a lost flip here scrolled the desktop's overlay the wrong way.
    #[test]
    fn a_native_wheel_crosses_in_the_pages_sign_both_ways() {
        assert_eq!(wheel_deltas(0.0, 40.0), (0.0, -40.0), "a positive native y crosses negative");
        assert_eq!(wheel_deltas(0.0, -40.0), (0.0, 40.0), "a negative native y crosses positive");
        assert_eq!(wheel_deltas(30.0, 0.0), (-30.0, 0.0), "a positive native x crosses negative");
        assert_eq!(wheel_deltas(-30.0, 0.0), (30.0, 0.0), "a negative native x crosses positive");
    }
}
