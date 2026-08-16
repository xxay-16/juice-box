//! Compatibility parser for legacy Java Juicebox saved-state XML files.

use std::{fs, path::Path};

use quick_xml::{Reader, escape::unescape, events::Event};
use thiserror::Error;

const STATE_FIELDS: [&str; 21] = [
    "MapPath",
    "Map",
    "MapURL",
    "ControlURL",
    "XChromosome",
    "YChromosome",
    "UnitName",
    "BinSize",
    "xOrigin",
    "yOrigin",
    "ScaleFactor",
    "DisplayOption",
    "NormalizationType",
    "MinColorVal",
    "LowerColorVal",
    "UpperColorVal",
    "MaxColorVal",
    "colrScaleVal",
    "LoadedTrackURLS",
    "LoadedTrackNames",
    "ConfigTrackInfo",
];

#[derive(Debug, Clone, PartialEq)]
pub struct LegacySessionState {
    pub id: String,
    pub map_path: String,
    pub map_title: String,
    pub map_urls: Vec<String>,
    pub control_urls: Vec<String>,
    pub x_chromosome: String,
    pub y_chromosome: String,
    pub unit: String,
    pub bin_size: u32,
    pub x_origin_bins: f64,
    pub y_origin_bins: f64,
    pub scale_factor: f64,
    pub display_option: String,
    pub normalization: String,
    pub min_color: f64,
    pub lower_color: f64,
    pub upper_color: f64,
    pub max_color: f64,
    pub color_scale_factor: f64,
    pub loaded_track_urls: String,
    pub loaded_track_names: String,
    pub track_config: String,
}

impl LegacySessionState {
    fn from_fields(
        selected_path: Option<String>,
        fields: &[Option<String>],
    ) -> Result<Self, SessionError> {
        let required = |index: usize| {
            fields[index]
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(SessionError::MissingField(STATE_FIELDS[index]))
        };
        let parse_u32 = |index: usize| -> Result<u32, SessionError> {
            required(index)?
                .trim()
                .parse()
                .map_err(|_| SessionError::InvalidField {
                    field: STATE_FIELDS[index],
                    value: required(index).unwrap_or_default().to_owned(),
                })
        };
        let parse_f64 = |index: usize| -> Result<f64, SessionError> {
            let value = required(index)?;
            value
                .trim()
                .parse()
                .map_err(|_| SessionError::InvalidField {
                    field: STATE_FIELDS[index],
                    value: value.to_owned(),
                })
        };
        let split_urls = |value: &str| {
            if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("null") {
                Vec::new()
            } else {
                value
                    .split("##")
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
        };
        let scale_factor = parse_f64(10)?;
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Err(SessionError::InvalidField {
                field: STATE_FIELDS[10],
                value: scale_factor.to_string(),
            });
        }
        Ok(Self {
            id: selected_path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(required(0)?)
                .to_owned(),
            map_path: required(0)?.to_owned(),
            map_title: required(1)?.to_owned(),
            map_urls: split_urls(required(2)?),
            control_urls: split_urls(fields[3].as_deref().unwrap_or("null")),
            x_chromosome: required(4)?.to_owned(),
            y_chromosome: required(5)?.to_owned(),
            unit: required(6)?.to_owned(),
            bin_size: parse_u32(7)?,
            x_origin_bins: parse_f64(8)?,
            y_origin_bins: parse_f64(9)?,
            scale_factor,
            display_option: required(11)?.to_owned(),
            normalization: required(12)?.to_owned(),
            min_color: parse_f64(13)?,
            lower_color: parse_f64(14)?,
            upper_color: parse_f64(15)?,
            max_color: parse_f64(16)?,
            color_scale_factor: parse_f64(17)?,
            loaded_track_urls: fields[18]
                .as_deref()
                .map(str::trim)
                .unwrap_or("none")
                .to_owned(),
            loaded_track_names: fields[19]
                .as_deref()
                .map(str::trim)
                .unwrap_or("none")
                .to_owned(),
            track_config: fields[20]
                .as_deref()
                .map(str::trim)
                .unwrap_or("none")
                .to_owned(),
        })
    }
}

pub fn read_legacy_session(
    path: impl AsRef<Path>,
) -> Result<Vec<LegacySessionState>, SessionError> {
    let bytes = fs::read(path)?;
    parse_legacy_session(&bytes)
}

pub fn parse_legacy_session(bytes: &[u8]) -> Result<Vec<LegacySessionState>, SessionError> {
    let mut reader = Reader::from_reader(bytes);
    // Java's DOM `getTextContent()` preserves whitespace across adjacent text,
    // entity, and CDATA nodes. Trimming each streaming event independently
    // would collapse `name &amp; <![CDATA[value]]>` into `name&value`.
    reader.config_mut().trim_text(false);
    let mut states = Vec::new();
    let mut fields = vec![None; STATE_FIELDS.len()];
    let mut selected_path = None;
    let mut in_state = false;
    let mut active_field = None;
    loop {
        match reader.read_event()? {
            Event::Start(start) if start.name().as_ref() == b"STATE" => {
                if in_state {
                    return Err(SessionError::NestedState);
                }
                in_state = true;
                fields.fill(None);
                selected_path = None;
                for attribute in start.attributes() {
                    let attribute = attribute.map_err(SessionError::Attribute)?;
                    if attribute.key.as_ref() == b"SelectedPath" {
                        selected_path = Some(
                            attribute
                                .decode_and_unescape_value(reader.decoder())?
                                .into_owned(),
                        );
                    }
                }
            }
            Event::Start(start) if in_state => {
                active_field = STATE_FIELDS
                    .iter()
                    .position(|field| start.name().as_ref() == field.as_bytes());
            }
            Event::Text(text) if in_state && active_field.is_some() => {
                let decoded = text.decode()?;
                let value = unescape(&decoded)?.into_owned();
                let field = &mut fields[active_field.expect("checked above")];
                field.get_or_insert_with(String::new).push_str(&value);
            }
            Event::CData(text) if in_state && active_field.is_some() => {
                let value = text.decode()?.into_owned();
                let field = &mut fields[active_field.expect("checked above")];
                field.get_or_insert_with(String::new).push_str(&value);
            }
            Event::GeneralRef(reference) if in_state && active_field.is_some() => {
                let reference = reference.decode()?;
                let escaped = format!("&{reference};");
                let value = unescape(&escaped)?.into_owned();
                let field = &mut fields[active_field.expect("checked above")];
                field.get_or_insert_with(String::new).push_str(&value);
            }
            Event::End(end) if end.name().as_ref() == b"STATE" => {
                if !in_state {
                    return Err(SessionError::UnexpectedStateEnd);
                }
                states.push(LegacySessionState::from_fields(
                    selected_path.take(),
                    &fields,
                )?);
                in_state = false;
                active_field = None;
            }
            Event::End(_) => active_field = None,
            Event::Eof => break,
            _ => {}
        }
    }
    if in_state {
        return Err(SessionError::UnclosedState);
    }
    if states.is_empty() {
        return Err(SessionError::NoStates);
    }
    Ok(states)
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Xml(#[from] quick_xml::Error),
    #[error(transparent)]
    Encoding(#[from] quick_xml::encoding::EncodingError),
    #[error(transparent)]
    Escape(#[from] quick_xml::escape::EscapeError),
    #[error("invalid XML attribute: {0}")]
    Attribute(quick_xml::events::attributes::AttrError),
    #[error("legacy session has no STATE elements")]
    NoStates,
    #[error("legacy session STATE is missing {0}")]
    MissingField(&'static str),
    #[error("legacy session field {field} has invalid value {value:?}")]
    InvalidField { field: &'static str, value: String },
    #[error("legacy session contains a nested STATE")]
    NestedState,
    #[error("legacy session contains an unexpected STATE end")]
    UnexpectedStateEnd,
    #[error("legacy session STATE is not closed")]
    UnclosedState,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<SavedMaps><STATE SelectedPath="demo">
<MapPath>demo</MapPath><Map>Genome (control=Control)</Map>
<MapURL>D:\data\genome.hic</MapURL><ControlURL>D:\data\control.hic</ControlURL>
<XChromosome>assembly</XChromosome><YChromosome>assembly</YChromosome>
<UnitName>BP</UnitName><BinSize>500000</BinSize><xOrigin>12.5</xOrigin>
<yOrigin>25</yOrigin><ScaleFactor>2</ScaleFactor><DisplayOption>NORM2OBSVSCTRL</DisplayOption>
<NormalizationType>KR</NormalizationType><MinColorVal>0</MinColorVal>
<LowerColorVal>1</LowerColorVal><UpperColorVal>5</UpperColorVal><MaxColorVal>10</MaxColorVal>
<colrScaleVal>1</colrScaleVal><LoadedTrackURLS>none</LoadedTrackURLS>
<LoadedTrackNames>none</LoadedTrackNames><ConfigTrackInfo>none</ConfigTrackInfo>
</STATE></SavedMaps>"#;

    #[test]
    fn parses_all_java_state_fields() {
        let states = parse_legacy_session(FIXTURE.as_bytes()).unwrap();
        assert_eq!(states.len(), 1);
        let state = &states[0];
        assert_eq!(state.id, "demo");
        assert_eq!(state.map_path, "demo");
        assert_eq!(state.map_urls, [r"D:\data\genome.hic"]);
        assert_eq!(state.control_urls, [r"D:\data\control.hic"]);
        assert_eq!(state.bin_size, 500_000);
        assert_eq!(state.x_origin_bins, 12.5);
        assert_eq!(state.scale_factor, 2.0);
        assert_eq!(state.display_option, "NORM2OBSVSCTRL");
    }

    #[test]
    fn splits_java_multi_map_delimiter_and_accepts_null_control() {
        let xml = FIXTURE
            .replace("D:\\data\\genome.hic", "a.hic##b.hic")
            .replace("D:\\data\\control.hic", "null");
        let state = parse_legacy_session(xml.as_bytes()).unwrap().remove(0);
        assert_eq!(state.map_urls, ["a.hic", "b.hic"]);
        assert!(state.control_urls.is_empty());
    }

    #[test]
    fn rejects_non_positive_scale() {
        let xml = FIXTURE.replace(
            "<ScaleFactor>2</ScaleFactor>",
            "<ScaleFactor>0</ScaleFactor>",
        );
        assert!(matches!(
            parse_legacy_session(xml.as_bytes()),
            Err(SessionError::InvalidField {
                field: "ScaleFactor",
                ..
            })
        ));
    }

    #[test]
    fn preserves_selected_path_separately_from_map_path() {
        let xml = FIXTURE
            .replace("SelectedPath=\"demo\"", "SelectedPath=\"menu &amp; state\"")
            .replace(
                "<MapPath>demo</MapPath>",
                "<MapPath>internal-path</MapPath>",
            );
        let state = parse_legacy_session(xml.as_bytes()).unwrap().remove(0);
        assert_eq!(state.id, "menu & state");
        assert_eq!(state.map_path, "internal-path");
    }

    #[test]
    fn joins_text_entities_and_cdata_like_dom_text_content() {
        let xml = FIXTURE.replace(
            "<Map>Genome (control=Control)</Map>",
            "<Map>Genome &amp; <![CDATA[Control <A>]]></Map>",
        );
        let state = parse_legacy_session(xml.as_bytes()).unwrap().remove(0);
        assert_eq!(state.map_title, "Genome & Control <A>");
    }

    #[test]
    fn parses_multiple_states_without_leaking_fields() {
        let second = FIXTURE
            .replace("SelectedPath=\"demo\"", "SelectedPath=\"second\"")
            .replace("<MapPath>demo</MapPath>", "<MapPath>second-map</MapPath>")
            .replace("D:\\data\\genome.hic", "second.hic");
        let xml = format!(
            "<SavedMaps>{}{}</SavedMaps>",
            FIXTURE
                .split_once("<SavedMaps>")
                .unwrap()
                .1
                .trim_end_matches("</SavedMaps>"),
            second
                .split_once("<SavedMaps>")
                .unwrap()
                .1
                .trim_end_matches("</SavedMaps>")
        );
        let states = parse_legacy_session(xml.as_bytes()).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].id, "demo");
        assert_eq!(states[1].id, "second");
        assert_eq!(states[1].map_path, "second-map");
    }
}
