//! Reading the `hints` dictionary of a `Notify` call.
//!
//! This is the protocol edge: `a{sv}` comes in as loosely typed values and
//! leaves as a [`Hints`] struct that the rest of the daemon can rely on.
//! Senders disagree about the details — an urgency arrives as a byte, an int,
//! or a uint; a transient flag as a bool or a number; a value is sometimes
//! wrapped in a second variant — so every read is tolerant, and anything
//! unreadable is simply absent rather than fatal.

use std::collections::HashMap;
use std::sync::Arc;

use zbus::zvariant::{OwnedValue, Value};

use super::model::{ImageData, Urgency};

/// Everything the daemon reads out of the hints dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    /// The `urgency` hint.
    pub urgency: Urgency,
    /// The `transient` hint: a banner and nothing more.
    pub transient: bool,
    /// The sender asks the daemon not to play any sound.
    pub suppress_sound: bool,
    /// The `desktop-entry` hint.
    pub desktop_entry: Option<String>,
    /// The `image-path` hint, under either of its two spellings.
    pub image_path: Option<String>,
    /// Pixels from `image-data`, if they were coherent.
    pub image_data: Option<Arc<ImageData>>,
}

impl Hints {
    /// Read a hints dictionary.
    pub fn parse(hints: &HashMap<String, OwnedValue>) -> Self {
        let mut parsed = Self::default();

        for (key, value) in hints {
            let value = unwrap_variants(value);
            match key.as_str() {
                "urgency" => {
                    if let Some(urgency) = as_u8(value) {
                        parsed.urgency = Urgency::from_wire(urgency);
                    }
                }
                "transient" => parsed.transient = as_bool(value).unwrap_or(false),
                "suppress-sound" => parsed.suppress_sound = as_bool(value).unwrap_or(false),
                "desktop-entry" => parsed.desktop_entry = as_string(value),
                // The specification renamed these between 1.1 and 1.2 and
                // senders never fully caught up, so both spellings are read.
                // The current name wins when a sender manages to send both.
                "image-path" => parsed.image_path = as_string(value).or(parsed.image_path.take()),
                "image_path" if parsed.image_path.is_none() => parsed.image_path = as_string(value),
                "image-data" => parsed.image_data = as_image(value).or(parsed.image_data.take()),
                // `icon_data` is the 1.0 spelling, and the lowest priority of
                // the three: it is what a sender falls back to.
                "image_data" | "icon_data" if parsed.image_data.is_none() => {
                    parsed.image_data = as_image(value);
                }
                _ => {}
            }
        }

        parsed
    }
}

/// Strip any number of variant wrappers from a value.
///
/// A dictionary of `a{sv}` already unwraps one level, but senders that build
/// the dictionary by hand sometimes put a variant inside the variant.
fn unwrap_variants<'v>(value: &'v Value<'v>) -> &'v Value<'v> {
    let mut value = value;
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

/// Read a small unsigned number from any of the integer types.
fn as_u8(value: &Value<'_>) -> Option<u8> {
    let number = as_i64(value)?;
    Some(number.clamp(0, 2) as u8)
}

/// Read any integer as an `i64`.
fn as_i64(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::U8(v) => Some(i64::from(*v)),
        Value::I16(v) => Some(i64::from(*v)),
        Value::U16(v) => Some(i64::from(*v)),
        Value::I32(v) => Some(i64::from(*v)),
        Value::U32(v) => Some(i64::from(*v)),
        Value::I64(v) => Some(*v),
        Value::U64(v) => i64::try_from(*v).ok(),
        _ => None,
    }
}

/// Read a flag sent as a boolean or as any non-zero number.
fn as_bool(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        other => as_i64(other).map(|number| number != 0),
    }
}

/// Read a non-empty string.
fn as_string(value: &Value<'_>) -> Option<String> {
    let text = match value {
        Value::Str(text) => text.as_str(),
        Value::ObjectPath(path) => path.as_str(),
        Value::Signature(signature) => {
            return Some(signature.to_string()).filter(|s| !s.is_empty());
        }
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Read the `(iiibiiay)` image structure.
///
/// An image whose buffer does not cover the geometry it claims is dropped:
/// turning it into a texture would read past the end of the allocation.
fn as_image(value: &Value<'_>) -> Option<Arc<ImageData>> {
    let Value::Structure(structure) = value else {
        return None;
    };
    let fields = structure.fields();
    let [
        width,
        height,
        rowstride,
        has_alpha,
        bits_per_sample,
        channels,
        data,
    ] = fields
    else {
        return None;
    };

    let image = ImageData {
        width: as_i64(unwrap_variants(width))?.try_into().ok()?,
        height: as_i64(unwrap_variants(height))?.try_into().ok()?,
        rowstride: as_i64(unwrap_variants(rowstride))?.try_into().ok()?,
        has_alpha: as_bool(unwrap_variants(has_alpha))?,
        bits_per_sample: as_i64(unwrap_variants(bits_per_sample))?.try_into().ok()?,
        channels: as_i64(unwrap_variants(channels))?.try_into().ok()?,
        data: as_bytes(unwrap_variants(data))?,
    };

    image.is_coherent().then(|| Arc::new(image))
}

/// Read an `ay` byte array.
fn as_bytes(value: &Value<'_>) -> Option<Vec<u8>> {
    let Value::Array(array) = value else {
        return None;
    };
    array
        .iter()
        .map(|byte| match unwrap_variants(byte) {
            Value::U8(byte) => Some(*byte),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Signature, Structure, StructureBuilder};

    fn hints<'a>(
        pairs: impl IntoIterator<Item = (&'a str, Value<'a>)>,
    ) -> HashMap<String, OwnedValue> {
        pairs
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string(),
                    OwnedValue::try_from(value).expect("a hint value must be ownable"),
                )
            })
            .collect()
    }

    /// A valid `(iiibiiay)` image structure of `width` x `height` RGBA pixels.
    fn image_struct(width: i32, height: i32, channels: i32) -> Structure<'static> {
        let rowstride = width * channels;
        let bytes: Vec<u8> = vec![0xab; (rowstride * height) as usize];
        StructureBuilder::new()
            .add_field(width)
            .add_field(height)
            .add_field(rowstride)
            .add_field(channels == 4)
            .add_field(8i32)
            .add_field(channels)
            .add_field(Array::from(bytes))
            .build()
            .expect("a well-formed image structure")
    }

    #[test]
    fn an_empty_dictionary_is_all_defaults() {
        let parsed = Hints::parse(&HashMap::new());
        assert_eq!(parsed, Hints::default());
        assert_eq!(parsed.urgency, Urgency::Normal);
        assert!(!parsed.transient);
    }

    #[test]
    fn urgency_is_read_from_every_integer_type_senders_use() {
        for value in [
            Value::U8(2),
            Value::I32(2),
            Value::U32(2),
            Value::I16(2),
            Value::I64(2),
        ] {
            let parsed = Hints::parse(&hints([("urgency", value.clone())]));
            assert_eq!(parsed.urgency, Urgency::Critical, "{value:?}");
        }

        assert_eq!(
            Hints::parse(&hints([("urgency", Value::U8(0))])).urgency,
            Urgency::Low
        );
        assert_eq!(
            Hints::parse(&hints([("urgency", Value::I32(99))])).urgency,
            Urgency::Critical,
            "out-of-range urgency clamps rather than being ignored"
        );
        assert_eq!(
            Hints::parse(&hints([("urgency", Value::from("high"))])).urgency,
            Urgency::Normal,
            "a hint of the wrong type falls back to the default"
        );
    }

    #[test]
    fn transient_accepts_a_boolean_or_a_number() {
        for value in [
            Value::Bool(true),
            Value::U8(1),
            Value::I32(-3),
            Value::U32(7),
        ] {
            assert!(
                Hints::parse(&hints([("transient", value.clone())])).transient,
                "{value:?}"
            );
        }
        for value in [Value::Bool(false), Value::U8(0), Value::I32(0)] {
            assert!(
                !Hints::parse(&hints([("transient", value.clone())])).transient,
                "{value:?}"
            );
        }
    }

    #[test]
    fn suppress_sound_is_tolerant_and_defaults_to_false() {
        assert!(!Hints::parse(&HashMap::new()).suppress_sound);
        for (value, expected) in [
            (Value::Bool(true), true),
            (Value::Bool(false), false),
            (Value::I32(1), true),
            (Value::I32(0), false),
            (Value::from("true"), false),
            (Value::Value(Box::new(Value::Bool(true))), true),
        ] {
            assert_eq!(
                Hints::parse(&hints([("suppress-sound", value)])).suppress_sound,
                expected
            );
        }
    }

    #[test]
    fn a_value_wrapped_in_a_second_variant_is_still_read() {
        let nested = Value::Value(Box::new(Value::Value(Box::new(Value::U8(2)))));
        assert_eq!(
            Hints::parse(&hints([("urgency", nested)])).urgency,
            Urgency::Critical
        );
    }

    #[test]
    fn both_spellings_of_image_path_are_accepted() {
        assert_eq!(
            Hints::parse(&hints([("image-path", Value::from("/tmp/a.png"))])).image_path,
            Some("/tmp/a.png".to_string())
        );
        assert_eq!(
            Hints::parse(&hints([("image_path", Value::from("/tmp/b.png"))])).image_path,
            Some("/tmp/b.png".to_string())
        );

        let both = Hints::parse(&hints([
            ("image_path", Value::from("/tmp/old.png")),
            ("image-path", Value::from("/tmp/new.png")),
        ]));
        assert_eq!(
            both.image_path,
            Some("/tmp/new.png".to_string()),
            "the current spelling wins"
        );
    }

    #[test]
    fn a_blank_string_hint_counts_as_absent() {
        let parsed = Hints::parse(&hints([
            ("desktop-entry", Value::from("   ")),
            ("image-path", Value::from("")),
        ]));
        assert_eq!(parsed.desktop_entry, None);
        assert_eq!(parsed.image_path, None);
    }

    #[test]
    fn desktop_entry_is_trimmed() {
        assert_eq!(
            Hints::parse(&hints([(
                "desktop-entry",
                Value::from(" org.gnome.Fractal ")
            )]))
            .desktop_entry,
            Some("org.gnome.Fractal".to_string())
        );
    }

    #[test]
    fn image_data_is_read_under_all_three_of_its_names() {
        for key in ["image-data", "image_data", "icon_data"] {
            let parsed = Hints::parse(&hints([(key, Value::Structure(image_struct(2, 2, 4)))]));
            let image = parsed
                .image_data
                .unwrap_or_else(|| panic!("{key} should parse"));
            assert_eq!(image.width, 2);
            assert_eq!(image.height, 2);
            assert_eq!(image.channels, 4);
            assert!(image.has_alpha);
            assert_eq!(image.data.len(), 16);
        }
    }

    #[test]
    fn the_current_image_data_spelling_wins_over_the_older_ones() {
        let parsed = Hints::parse(&hints([
            ("icon_data", Value::Structure(image_struct(1, 1, 4))),
            ("image-data", Value::Structure(image_struct(8, 8, 4))),
        ]));
        assert_eq!(parsed.image_data.expect("an image").width, 8);
    }

    #[test]
    fn three_channel_images_without_alpha_are_accepted() {
        let parsed = Hints::parse(&hints([(
            "image-data",
            Value::Structure(image_struct(4, 3, 3)),
        )]));
        let image = parsed.image_data.expect("an RGB image");
        assert_eq!(image.channels, 3);
        assert!(!image.has_alpha);
        assert_eq!(image.data.len(), 36);
    }

    #[test]
    fn an_image_whose_buffer_is_too_short_is_dropped() {
        let lying = StructureBuilder::new()
            .add_field(64i32)
            .add_field(64i32)
            .add_field(256i32)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(Array::from(vec![0u8; 16]))
            .build()
            .expect("structure");
        assert_eq!(
            Hints::parse(&hints([("image-data", Value::Structure(lying))])).image_data,
            None,
            "a texture built from this would read past the buffer"
        );
    }

    #[test]
    fn an_image_structure_of_the_wrong_shape_is_ignored() {
        let short = StructureBuilder::new()
            .add_field(2i32)
            .add_field(2i32)
            .build()
            .expect("structure");
        assert_eq!(
            Hints::parse(&hints([("image-data", Value::Structure(short))])).image_data,
            None
        );
        assert_eq!(
            Hints::parse(&hints([("image-data", Value::from("not a struct"))])).image_data,
            None
        );
    }

    #[test]
    fn unknown_hints_are_ignored_rather_than_failing_the_call() {
        let parsed = Hints::parse(&hints([
            ("x-kde-something", Value::from("whatever")),
            ("sound-name", Value::from("message-new-instant")),
            ("sound-file", Value::from("/tmp/untrusted.oga")),
            ("urgency", Value::U8(2)),
            (
                "signature",
                Value::Signature(Signature::from_bytes(b"a{sv}").expect("a valid signature")),
            ),
        ]));
        assert_eq!(parsed.urgency, Urgency::Critical);
    }

    #[test]
    fn a_realistic_telegram_notification_parses_whole() {
        let parsed = Hints::parse(&hints([
            ("urgency", Value::U8(1)),
            ("desktop-entry", Value::from("org.telegram.desktop")),
            ("image-data", Value::Structure(image_struct(64, 64, 4))),
            ("sound-name", Value::from("message-new-instant")),
        ]));

        assert_eq!(parsed.urgency, Urgency::Normal);
        assert!(!parsed.transient);
        assert_eq!(
            parsed.desktop_entry.as_deref(),
            Some("org.telegram.desktop")
        );
        assert_eq!(parsed.image_data.expect("avatar pixels").width, 64);
    }
}
