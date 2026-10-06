//! A font family the reader chose by one of its files (Settings, Appearance,
//! Font). A variable face draws every weight; static faces of the same
//! family are found beside the chosen file, and each weight the interface
//! draws takes the nearest one, as browsers match `font-weight`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fastframe_fonts::Weight;
use skrifa::MetadataProvider;

/// Font files are recognised by these extensions when looking beside the
/// chosen file.
const EXTENSIONS: [&str; 4] = ["ttf", "otf", "ttc", "otc"];

/// The most sibling files described while looking for the rest of a
/// family, so a file picked from a folder holding every installed font
/// stays quick.
const MAX_SIBLINGS: usize = 32;

/// The tables that name and weigh a face, in tag order: enough to describe
/// it without reading its outlines.
const NAMING_TABLES: [&[u8; 4]; 5] = [b"OS/2", b"fvar", b"head", b"name", b"post"];

/// The largest naming table copied; a real one is a few kilobytes.
const MAX_NAMING_TABLE: usize = 4 << 20;

/// A face of the family read in full: its file, the face inside it, its
/// weight, and whether a `wght` axis lets it draw every weight.
struct Face {
    bytes: Arc<Vec<u8>>,
    index: u32,
    weight: f32,
    variable: bool,
}

/// What a face says about itself, enough to choose it.
#[derive(Clone)]
struct Description {
    path: PathBuf,
    index: u32,
    family: String,
    upright: bool,
    weight: f32,
    variable: bool,
}

/// The chosen family's faces that draw the interface's weights, each
/// checked to parse as egui will.
pub struct Family {
    pub name: String,
    faces: Vec<Face>,
}

impl Family {
    /// Reads the chosen file and finds the upright faces of the same family
    /// in its folder. Only the faces some weight draws with are read in
    /// full. Fails when the file cannot be read or holds no usable font.
    pub fn load(path: &Path) -> Result<Self, String> {
        let chosen = describe(path)?;
        let Some(first) = chosen
            .iter()
            .find(|face| face.upright)
            .or(chosen.first())
            .cloned()
        else {
            return Err("no font in this file".to_owned());
        };
        let consider = |candidates: &mut Vec<Description>, found: Vec<Description>| {
            for face in found {
                if face.family == first.family
                    && face.upright
                    && !candidates.iter().any(|known| known.weight == face.weight)
                {
                    candidates.push(face);
                }
            }
        };
        let mut candidates = Vec::new();
        consider(&mut candidates, chosen);
        if candidates.is_empty() {
            // An italic file on its own still draws, slanted.
            candidates.push(first.clone());
        }
        if !candidates.iter().any(|face| face.variable) {
            for sibling in siblings(path) {
                if let Ok(found) = describe(&sibling) {
                    consider(&mut candidates, found);
                }
            }
        }
        // A face egui cannot parse is dropped, and the weights choose again.
        let mut files: HashMap<PathBuf, Arc<Vec<u8>>> = HashMap::new();
        loop {
            let mut faces = Vec::new();
            let mut failed = None;
            for face in drawing(&candidates) {
                let bytes = match files.get(&face.path) {
                    Some(bytes) => Arc::clone(bytes),
                    None => match std::fs::read(&face.path) {
                        Ok(bytes) => Arc::clone(
                            files
                                .entry(face.path.clone())
                                .or_insert_with(|| Arc::new(bytes)),
                        ),
                        Err(_) => {
                            failed = Some(face);
                            break;
                        }
                    },
                };
                if skrifa::FontRef::from_index(&bytes, face.index).is_err() {
                    failed = Some(face);
                    break;
                }
                faces.push(Face {
                    bytes,
                    index: face.index,
                    weight: face.weight,
                    variable: face.variable,
                });
            }
            let Some(failed) = failed else {
                return Ok(Self {
                    name: first.family,
                    faces,
                });
            };
            candidates.retain(|face| face.path != failed.path || face.index != failed.index);
            if candidates.is_empty() {
                return Err("not a font file".to_owned());
            }
        }
    }

    /// Whether every weight draws the same, so bold text looks regular.
    pub fn single_weight(&self) -> bool {
        self.faces.len() == 1 && !self.faces[0].variable
    }

    /// The face that draws `weight`.
    fn face_for(&self, weight: Weight) -> &Face {
        if let Some(face) = self.faces.iter().find(|face| face.variable) {
            return face;
        }
        let weights: Vec<f32> = self.faces.iter().map(|face| face.weight).collect();
        let nearest = nearest_weight(&weights, weight.value()).unwrap_or(weights[0]);
        self.faces
            .iter()
            .find(|face| face.weight == nearest)
            .unwrap_or(&self.faces[0])
    }

    /// The face that draws `weight`, ready for egui: a variable face set to
    /// the weight on its `wght` axis.
    pub fn font_data(&self, weight: Weight) -> egui::FontData {
        let face = self.face_for(weight);
        let mut data = egui::FontData::from_owned(face.bytes.to_vec());
        data.index = face.index;
        if face.variable {
            data.tweak.coords =
                egui::epaint::text::VariationCoords::new([(b"wght", weight.value())]);
        }
        data
    }

    /// The weight each interface weight is drawn at, for tests.
    #[cfg(test)]
    fn drawn_weights(&self) -> Vec<f32> {
        Weight::ALL
            .iter()
            .map(|weight| self.face_for(*weight).weight)
            .collect()
    }
}

/// The faces among `candidates` that some interface weight draws with: a
/// variable face alone, or each weight's nearest static face once.
fn drawing(candidates: &[Description]) -> Vec<Description> {
    if let Some(face) = candidates.iter().find(|face| face.variable) {
        return vec![face.clone()];
    }
    let weights: Vec<f32> = candidates.iter().map(|face| face.weight).collect();
    let mut drawing: Vec<Description> = Vec::new();
    for weight in Weight::ALL {
        if let Some(nearest) = nearest_weight(&weights, weight.value())
            && !drawing.iter().any(|face| face.weight == nearest)
            && let Some(face) = candidates.iter().find(|face| face.weight == nearest)
        {
            drawing.push(face.clone());
        }
    }
    drawing
}

/// Every face in a font file or collection, described from its naming
/// tables alone.
fn describe(path: &Path) -> Result<Vec<Description>, String> {
    let tables = naming_tables(path)?;
    let file = skrifa::raw::FileRef::new(&tables).map_err(|error| error.to_string())?;
    let mut faces = Vec::new();
    for (index, font) in file.fonts().enumerate() {
        let (Ok(font), Ok(index)) = (font, u32::try_from(index)) else {
            continue;
        };
        let Some(family) = family_name(&font) else {
            continue;
        };
        let attributes = font.attributes();
        faces.push(Description {
            path: path.to_owned(),
            index,
            family,
            upright: attributes.style == skrifa::attribute::Style::Normal,
            weight: attributes.weight.value(),
            variable: font
                .axes()
                .iter()
                .any(|axis| axis.tag() == skrifa::Tag::new(b"wght")),
        });
    }
    if faces.is_empty() {
        return Err("not a font file".to_owned());
    }
    Ok(faces)
}

/// The [`NAMING_TABLES`] of a font file, copied into a small font of their
/// own, so describing a 14 MB file reads a few kilobytes. A collection is
/// read whole: its faces share tables, and collections are rare.
fn naming_tables(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let error = |error: std::io::Error| error.to_string();
    let mut file = std::fs::File::open(path).map_err(error)?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(error)?;
    if &header[..4] == b"ttcf" {
        return std::fs::read(path).map_err(error);
    }
    let count = usize::from(u16::from_be_bytes([header[4], header[5]]));
    let mut records = vec![0u8; count * 16];
    file.read_exact(&mut records).map_err(error)?;
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    for record in records.as_chunks::<16>().0 {
        let tag: [u8; 4] = record[..4].try_into().unwrap_or_default();
        if !NAMING_TABLES.contains(&&tag) {
            continue;
        }
        let offset = u32::from_be_bytes(record[8..12].try_into().unwrap_or_default());
        let length = u32::from_be_bytes(record[12..16].try_into().unwrap_or_default());
        let Ok(length) = usize::try_from(length) else {
            continue;
        };
        if length > MAX_NAMING_TABLE {
            continue;
        }
        let mut data = vec![0u8; length];
        file.seek(SeekFrom::Start(u64::from(offset)))
            .map_err(error)?;
        file.read_exact(&mut data).map_err(error)?;
        tables.push((tag, data));
    }
    // The table directory is searched by tag, so it stays in tag order.
    tables.sort_by_key(|(tag, _)| *tag);
    let mut font = Vec::new();
    font.extend_from_slice(&header[..4]);
    let count = u16::try_from(tables.len()).unwrap_or_default();
    font.extend_from_slice(&count.to_be_bytes());
    font.extend_from_slice(&[0; 6]);
    let mut offset = 12 + tables.len() * 16;
    for (tag, data) in &tables {
        font.extend_from_slice(tag);
        font.extend_from_slice(&[0; 4]);
        font.extend_from_slice(&u32::try_from(offset).unwrap_or_default().to_be_bytes());
        font.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        font.extend_from_slice(data);
        font.resize(font.len().next_multiple_of(4), 0);
    }
    Ok(font)
}

/// The family a face belongs to: its typographic family name, which groups
/// every weight, or its legacy family name when it has none.
fn family_name(font: &skrifa::FontRef) -> Option<String> {
    [
        skrifa::string::StringId::TYPOGRAPHIC_FAMILY_NAME,
        skrifa::string::StringId::FAMILY_NAME,
    ]
    .into_iter()
    .find_map(|id| {
        font.localized_strings(id)
            .english_or_first()
            .map(|name| name.to_string())
            .filter(|name| !name.is_empty())
    })
}

/// Font files beside `path` whose names start like its own: up to its last
/// `-`, `_` or space (`Inter-Regular` → `Inter-`), or the whole name when it
/// has none (`segoeui` → `segoeuib`). Sorted, so the same files win each
/// time, and capped at [`MAX_SIBLINGS`].
fn siblings(path: &Path) -> Vec<std::path::PathBuf> {
    let (Some(folder), Some(stem)) = (path.parent(), path.file_stem()) else {
        return Vec::new();
    };
    let stem = stem.to_string_lossy().to_lowercase();
    let prefix = stem
        .rfind(['-', '_', ' '])
        .map_or(stem.as_str(), |at| &stem[..=at]);
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| candidate != path)
        .filter(|candidate| {
            candidate.extension().is_some_and(|extension| {
                EXTENSIONS.contains(&extension.to_string_lossy().to_lowercase().as_str())
            }) && candidate
                .file_stem()
                .is_some_and(|name| name.to_string_lossy().to_lowercase().starts_with(prefix))
        })
        .collect();
    found.sort();
    found.truncate(MAX_SIBLINGS);
    found
}

/// The weight among `weights` that draws `target`, by CSS's font matching:
/// the exact weight; for 400 to 500, heavier up to 500, then lighter, then
/// heavier; below 400, lighter then heavier; above 500, heavier then lighter.
fn nearest_weight(weights: &[f32], target: f32) -> Option<f32> {
    let rank = |weight: f32| -> (u8, f32) {
        let distance = (weight - target).abs();
        if weight == target {
            (0, 0.0)
        } else if (400.0..=500.0).contains(&target) {
            if weight > target && weight <= 500.0 {
                (1, distance)
            } else if weight < target {
                (2, distance)
            } else {
                (3, distance)
            }
        } else if (target < 400.0) == (weight < target) {
            (1, distance)
        } else {
            (2, distance)
        }
    };
    weights.iter().copied().min_by(|a, b| {
        rank(*a)
            .partial_cmp(&rank(*b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_weight_takes_the_nearest_face_as_browsers_do() {
        // Regular and bold: medium draws regular, semibold draws bold.
        assert_eq!(nearest_weight(&[400.0, 700.0], 500.0), Some(400.0));
        assert_eq!(nearest_weight(&[400.0, 700.0], 600.0), Some(700.0));
        // 400 prefers 500 before anything lighter.
        assert_eq!(nearest_weight(&[300.0, 500.0], 400.0), Some(500.0));
        // Bold prefers heavier, then the nearest lighter.
        assert_eq!(nearest_weight(&[400.0, 600.0, 900.0], 700.0), Some(900.0));
        assert_eq!(nearest_weight(&[400.0, 600.0], 700.0), Some(600.0));
        // Below 400 prefers lighter.
        assert_eq!(nearest_weight(&[200.0, 400.0], 300.0), Some(200.0));
        assert_eq!(nearest_weight(&[], 400.0), None);
    }

    /// A copy of a static font that claims another weight in its OS/2
    /// table, standing in for a sibling file of the same family.
    fn with_weight(font: &[u8], weight: u16) -> Vec<u8> {
        let mut bytes = font.to_vec();
        let tables = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        for record in (0..tables).map(|table| 12 + table * 16) {
            if &bytes[record..record + 4] == b"OS/2" {
                let at = u32::from_be_bytes(bytes[record + 8..record + 12].try_into().unwrap());
                let at = usize::try_from(at).unwrap() + 4;
                bytes[at..at + 2].copy_from_slice(&weight.to_be_bytes());
                return bytes;
            }
        }
        panic!("no OS/2 table");
    }

    fn egui_font(name: &str) -> Vec<u8> {
        egui::FontDefinitions::default().font_data[name]
            .font
            .to_vec()
    }

    #[test]
    fn a_static_file_brings_the_rest_of_its_family_from_its_folder() {
        let folder = tempfile::tempdir().unwrap();
        let light = egui_font("Ubuntu-Light");
        let chosen = folder.path().join("Ubuntu-Light.ttf");
        std::fs::write(&chosen, &light).unwrap();
        std::fs::write(
            folder.path().join("Ubuntu-Bold.ttf"),
            with_weight(&light, 700),
        )
        .unwrap();
        // No interface weight draws heavy, so it is never read in full.
        std::fs::write(
            folder.path().join("Ubuntu-Heavy.ttf"),
            with_weight(&light, 900),
        )
        .unwrap();
        // Another family that happens to share the name's start.
        std::fs::write(folder.path().join("Ubuntu-Mono.ttf"), egui_font("Hack")).unwrap();
        // Not beside the chosen file's name, so never read.
        std::fs::write(folder.path().join("Other.ttf"), with_weight(&light, 500)).unwrap();

        let family = Family::load(&chosen).unwrap();
        assert_eq!(family.name, "Ubuntu");
        assert!(!family.single_weight());
        // Regular and medium take the light face, semibold and bold the bold.
        assert_eq!(family.drawn_weights(), [300.0, 300.0, 700.0, 700.0]);
        assert_eq!(family.faces.len(), 2);
    }

    #[test]
    fn a_lone_static_file_draws_every_weight_and_says_so() {
        let folder = tempfile::tempdir().unwrap();
        let chosen = folder.path().join("Ubuntu-Light.ttf");
        std::fs::write(&chosen, egui_font("Ubuntu-Light")).unwrap();
        let family = Family::load(&chosen).unwrap();
        assert!(family.single_weight());
        assert_eq!(family.drawn_weights(), [300.0; 4]);
    }

    #[test]
    fn a_variable_file_draws_every_weight_on_its_own() {
        let folder = tempfile::tempdir().unwrap();
        let chosen = folder.path().join("Inter.ttf");
        std::fs::write(&chosen, fastframe_fonts::INTER).unwrap();
        let family = Family::load(&chosen).unwrap();
        assert!(!family.single_weight());
        let bold = family.font_data(Weight::Bold);
        assert_eq!(
            bold.tweak.coords,
            egui::epaint::text::VariationCoords::new([(b"wght", 700.0)])
        );
    }

    #[test]
    fn a_file_that_is_not_a_font_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let chosen = folder.path().join("Broken.ttf");
        std::fs::write(&chosen, b"not a font").unwrap();
        assert!(Family::load(&chosen).is_err());
    }
}
