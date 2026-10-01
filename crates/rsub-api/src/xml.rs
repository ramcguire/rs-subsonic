//! A serde `Serializer` that renders Subsonic-style XML.
//!
//! Mapping rules:
//!
//! | Rust field shape        | XML                              |
//! |-------------------------|----------------------------------|
//! | scalar                  | attribute `name="…"`             |
//! | `None` / unit           | omitted                          |
//! | struct                  | child element `<name …/>`        |
//! | `Vec<struct>`           | repeated `<name/><name/>`        |
//! | `Vec<scalar>`           | repeated `<name>text</name>`     |
//! | field named `value`     | element text content             |
//!
//! Attributes are written straight to the output; child elements are buffered
//! per element so that field order in the Rust struct does not matter.

use std::fmt::{self, Display};

use serde::ser::{self, Impossible, Serialize};

/// Field name whose scalar value is written as element text instead of an attribute.
const TEXT_FIELD: &str = "value";

#[derive(Debug)]
pub struct Error(String);

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

fn unsupported(what: &str) -> Error {
    Error(format!("xml: unsupported value kind: {what}"))
}

type Result<T> = std::result::Result<T, Error>;

/// Serialize `value` (which must serialize as a struct) as the root element `root`,
/// optionally adding an `xmlns` attribute.
pub fn to_writer<T: Serialize + ?Sized>(
    out: &mut Vec<u8>,
    root: &str,
    xmlns: Option<&str>,
    value: &T,
) -> Result<()> {
    value.serialize(RootSerializer { out, root, xmlns })
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

fn escape_into(out: &mut Vec<u8>, s: &str, attr: bool) {
    let bytes = s.as_bytes();
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let rep: &[u8] = match b {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'"' if attr => b"&quot;",
            b'\'' if attr => b"&apos;",
            b'\n' if attr => b"&#10;",
            b'\r' if attr => b"&#13;",
            b'\t' if attr => b"&#9;",
            // Control characters are not allowed in XML 1.0; drop them.
            0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F => b"",
            _ => continue,
        };
        out.extend_from_slice(&bytes[last..i]);
        out.extend_from_slice(rep);
        last = i + 1;
    }
    out.extend_from_slice(&bytes[last..]);
}

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

/// Converts a scalar into its textual form; errors for anything non-scalar.
struct ScalarSerializer;

macro_rules! scalar_display {
    ($($fn:ident: $ty:ty),*) => {
        $(fn $fn(self, v: $ty) -> Result<Option<String>> { Ok(Some(v.to_string())) })*
    };
}

impl ser::Serializer for ScalarSerializer {
    /// `None` means "omit" (unit / `Option::None`).
    type Ok = Option<String>;
    type Error = Error;
    type SerializeSeq = Impossible<Self::Ok, Error>;
    type SerializeTuple = Impossible<Self::Ok, Error>;
    type SerializeTupleStruct = Impossible<Self::Ok, Error>;
    type SerializeTupleVariant = Impossible<Self::Ok, Error>;
    type SerializeMap = Impossible<Self::Ok, Error>;
    type SerializeStruct = Impossible<Self::Ok, Error>;
    type SerializeStructVariant = Impossible<Self::Ok, Error>;

    scalar_display!(
        serialize_bool: bool, serialize_i8: i8, serialize_i16: i16, serialize_i32: i32,
        serialize_i64: i64, serialize_i128: i128, serialize_u8: u8, serialize_u16: u16,
        serialize_u32: u32, serialize_u64: u64, serialize_u128: u128, serialize_f32: f32,
        serialize_f64: f64, serialize_char: char
    );

    fn serialize_str(self, v: &str) -> Result<Self::Ok> {
        Ok(Some(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok> {
        Ok(Some(hex::encode(v)))
    }
    fn serialize_none(self) -> Result<Self::Ok> {
        Ok(None)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<Self::Ok> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<Self::Ok> {
        Ok(None)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Self::Ok> {
        Ok(None)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<Self::Ok> {
        Ok(Some(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<Self::Ok> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<Self::Ok> {
        Err(unsupported("newtype variant"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Err(unsupported("seq"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Err(unsupported("tuple"))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Err(unsupported("tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(unsupported("tuple variant"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(unsupported("map"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        Err(unsupported("struct"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(unsupported("struct variant"))
    }
}

// ---------------------------------------------------------------------------
// Elements
// ---------------------------------------------------------------------------

/// An open element: `<name attrs…` has been written to `out`; children and text
/// are collected in `body` until `end()`.
pub struct Element<'a> {
    out: &'a mut Vec<u8>,
    name: &'a str,
    body: Vec<u8>,
}

impl<'a> Element<'a> {
    fn open(out: &'a mut Vec<u8>, name: &'a str) -> Self {
        out.push(b'<');
        out.extend_from_slice(name.as_bytes());
        Element {
            out,
            name,
            body: Vec::new(),
        }
    }

    fn attr(&mut self, key: &str, value: &str) {
        self.out.push(b' ');
        self.out.extend_from_slice(key.as_bytes());
        self.out.extend_from_slice(b"=\"");
        escape_into(self.out, value, true);
        self.out.push(b'"');
    }

    fn close(self) {
        if self.body.is_empty() {
            self.out.extend_from_slice(b"/>");
        } else {
            self.out.push(b'>');
            self.out.extend_from_slice(&self.body);
            self.out.extend_from_slice(b"</");
            self.out.extend_from_slice(self.name.as_bytes());
            self.out.push(b'>');
        }
    }

    fn field<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> Result<()> {
        value.serialize(FieldSerializer { key, elem: self })
    }
}

impl ser::SerializeStruct for Element<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.field(key, value)
    }

    fn end(self) -> Result<()> {
        self.close();
        Ok(())
    }
}

/// Serializes the value of field `key` into `elem` (as attribute, text, or child element(s)).
struct FieldSerializer<'e, 'a> {
    key: &'e str,
    elem: &'e mut Element<'a>,
}

impl FieldSerializer<'_, '_> {
    fn scalar<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        if let Some(text) = v.serialize(ScalarSerializer)? {
            if self.key == TEXT_FIELD {
                escape_into(&mut self.elem.body, &text, false);
            } else {
                self.elem.attr(self.key, &text);
            }
        }
        Ok(())
    }
}

macro_rules! field_scalar {
    ($($fn:ident: $ty:ty),*) => {
        $(fn $fn(self, v: $ty) -> Result<()> { self.scalar(&v) })*
    };
}

impl<'e, 'a> ser::Serializer for FieldSerializer<'e, 'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SeqSerializer<'e>;
    type SerializeTuple = SeqSerializer<'e>;
    type SerializeTupleStruct = Impossible<(), Error>;
    type SerializeTupleVariant = Impossible<(), Error>;
    type SerializeMap = Impossible<(), Error>;
    type SerializeStruct = Element<'e>;
    type SerializeStructVariant = Impossible<(), Error>;

    field_scalar!(
        serialize_bool: bool, serialize_i8: i8, serialize_i16: i16, serialize_i32: i32,
        serialize_i64: i64, serialize_i128: i128, serialize_u8: u8, serialize_u16: u16,
        serialize_u32: u32, serialize_u64: u64, serialize_u128: u128, serialize_f32: f32,
        serialize_f64: f64, serialize_char: char, serialize_str: &str, serialize_bytes: &[u8]
    );

    fn serialize_none(self) -> Result<()> {
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<()> {
        self.scalar(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<()> {
        Err(unsupported("newtype variant"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        let elem = self.elem;
        Ok(SeqSerializer {
            key: self.key,
            body: &mut elem.body,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Err(unsupported("tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(unsupported("tuple variant"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(unsupported("map"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        let elem = self.elem;
        Ok(Element::open(&mut elem.body, self.key))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(unsupported("struct variant"))
    }
}

/// Each item of a sequence field becomes one `<key>` element in the parent's body.
pub struct SeqSerializer<'e> {
    key: &'e str,
    body: &'e mut Vec<u8>,
}

impl SeqSerializer<'_> {
    fn item<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(ItemSerializer {
            key: self.key,
            body: self.body,
        })
    }
}

impl ser::SerializeSeq for SeqSerializer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.item(v)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}

impl ser::SerializeTuple for SeqSerializer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.item(v)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}

/// Serializes one sequence item as a `<key>` element: struct → attributes/children, scalar → text.
struct ItemSerializer<'e> {
    key: &'e str,
    body: &'e mut Vec<u8>,
}

impl ItemSerializer<'_> {
    fn scalar<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        if let Some(text) = v.serialize(ScalarSerializer)? {
            let mut el = Element::open(self.body, self.key);
            escape_into(&mut el.body, &text, false);
            if el.body.is_empty() {
                // Keep empty strings as an explicit empty element rather than `<x/>`-vs-missing ambiguity.
                el.out.extend_from_slice(b"></");
                el.out.extend_from_slice(el.name.as_bytes());
                el.out.push(b'>');
            } else {
                el.close();
            }
        }
        Ok(())
    }
}

macro_rules! item_scalar {
    ($($fn:ident: $ty:ty),*) => {
        $(fn $fn(self, v: $ty) -> Result<()> { self.scalar(&v) })*
    };
}

impl<'e> ser::Serializer for ItemSerializer<'e> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Impossible<(), Error>;
    type SerializeTuple = Impossible<(), Error>;
    type SerializeTupleStruct = Impossible<(), Error>;
    type SerializeTupleVariant = Impossible<(), Error>;
    type SerializeMap = Impossible<(), Error>;
    type SerializeStruct = Element<'e>;
    type SerializeStructVariant = Impossible<(), Error>;

    item_scalar!(
        serialize_bool: bool, serialize_i8: i8, serialize_i16: i16, serialize_i32: i32,
        serialize_i64: i64, serialize_i128: i128, serialize_u8: u8, serialize_u16: u16,
        serialize_u32: u32, serialize_u64: u64, serialize_u128: u128, serialize_f32: f32,
        serialize_f64: f64, serialize_char: char, serialize_str: &str, serialize_bytes: &[u8]
    );

    fn serialize_none(self) -> Result<()> {
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<()> {
        self.scalar(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<()> {
        Err(unsupported("newtype variant"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Err(unsupported("nested seq"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Err(unsupported("nested tuple"))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Err(unsupported("tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(unsupported("tuple variant"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(unsupported("map"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        Ok(Element::open(self.body, self.key))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(unsupported("struct variant"))
    }
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

struct RootSerializer<'a> {
    out: &'a mut Vec<u8>,
    root: &'a str,
    xmlns: Option<&'a str>,
}

impl<'a> ser::Serializer for RootSerializer<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Impossible<(), Error>;
    type SerializeTuple = Impossible<(), Error>;
    type SerializeTupleStruct = Impossible<(), Error>;
    type SerializeTupleVariant = Impossible<(), Error>;
    type SerializeMap = Impossible<(), Error>;
    type SerializeStruct = Element<'a>;
    type SerializeStructVariant = Impossible<(), Error>;

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        let mut el = Element::open(self.out, self.root);
        if let Some(ns) = self.xmlns {
            el.attr("xmlns", ns);
        }
        Ok(el)
    }

    fn serialize_bool(self, _: bool) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_i8(self, _: i8) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_i16(self, _: i16) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_i32(self, _: i32) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_i64(self, _: i64) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_u8(self, _: u8) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_u16(self, _: u16) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_u32(self, _: u32) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_u64(self, _: u64) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_f32(self, _: f32) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_f64(self, _: f64) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_char(self, _: char) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_str(self, _: &str) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<()> {
        Err(unsupported("root scalar"))
    }
    fn serialize_none(self) -> Result<()> {
        Err(unsupported("root none"))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Err(unsupported("root unit"))
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<()> {
        Err(unsupported("root unit"))
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<()> {
        Err(unsupported("root unit variant"))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<()> {
        Err(unsupported("root newtype variant"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Err(unsupported("root seq"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Err(unsupported("root tuple"))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Err(unsupported("root tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(unsupported("root tuple variant"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(unsupported("root map"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(unsupported("root struct variant"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    fn render<T: Serialize>(v: &T) -> String {
        let mut out = Vec::new();
        to_writer(&mut out, "root", None, v).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[derive(Serialize)]
    struct Genre {
        #[serde(rename = "songCount")]
        song_count: u32,
        value: String,
    }

    #[derive(Serialize)]
    struct Child {
        id: String,
    }

    #[derive(Serialize)]
    struct Root {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        missing: Option<u32>,
        child: Vec<Child>,
        single: Child,
        folder: Vec<i32>,
        genre: Genre,
        flag: bool,
        gain: f64,
    }

    #[test]
    fn maps_fields_by_shape() {
        let xml = render(&Root {
            name: "a \"b\" & <c>".into(),
            missing: None,
            child: vec![Child { id: "1".into() }, Child { id: "2".into() }],
            single: Child { id: "s".into() },
            folder: vec![1, 2],
            genre: Genre {
                song_count: 3,
                value: "Rock & Roll".into(),
            },
            flag: true,
            gain: -6.5,
        });
        assert_eq!(
            xml,
            r#"<root name="a &quot;b&quot; &amp; &lt;c&gt;" flag="true" gain="-6.5"><child id="1"/><child id="2"/><single id="s"/><folder>1</folder><folder>2</folder><genre songCount="3">Rock &amp; Roll</genre></root>"#
        );
    }

    #[test]
    fn empty_struct_self_closes_and_control_chars_are_dropped() {
        #[derive(Serialize)]
        struct Empty {}
        #[derive(Serialize)]
        struct R {
            e: Empty,
            s: &'static str,
        }
        assert_eq!(
            render(&R {
                e: Empty {},
                s: "a\u{1}b\nc"
            }),
            r#"<root s="ab&#10;c"><e/></root>"#
        );
    }

    #[test]
    fn xmlns_on_root() {
        #[derive(Serialize)]
        struct R {}
        let mut out = Vec::new();
        to_writer(
            &mut out,
            "subsonic-response",
            Some("http://subsonic.org/restapi"),
            &R {},
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"<subsonic-response xmlns="http://subsonic.org/restapi"/>"#
        );
    }
}
