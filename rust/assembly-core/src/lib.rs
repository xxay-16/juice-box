//! UI-independent parser and domain model for Juicebox `.assembly` files.

use std::{collections::HashSet, fs, path::Path};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scaffold {
    pub name: String,
    pub id: u32,
    pub length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrientedScaffold {
    pub scaffold_id: u32,
    pub reversed: bool,
}

/// A scaffold placement in assembly coordinates.  Coordinates use the
/// half-open interval `[start, end)`, matching Rust ranges and the existing
/// Juicebox coordinate convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssemblyPlacement {
    pub superscaffold_index: usize,
    pub scaffold_index: usize,
    pub scaffold_id: u32,
    pub reversed: bool,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyDocument {
    pub version: u64,
    pub scaffolds: Vec<Scaffold>,
    pub superscaffolds: Vec<Vec<OrientedScaffold>>,
}

impl AssemblyDocument {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AssemblyError> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(text: &str) -> Result<Self, AssemblyError> {
        let mut scaffolds = Vec::new();
        let mut superscaffolds = Vec::new();
        let mut ids = HashSet::new();

        for (line_index, raw_line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix('>') {
                let mut fields = header.split_whitespace();
                let name = fields
                    .next()
                    .ok_or(AssemblyError::InvalidLine(line_number))?
                    .to_owned();
                let id = parse_field(fields.next(), line_number, "scaffold id")?;
                let length = parse_field(fields.next(), line_number, "scaffold length")?;
                if fields.next().is_some() || id == 0 || !ids.insert(id) {
                    return Err(AssemblyError::InvalidLine(line_number));
                }
                scaffolds.push(Scaffold { name, id, length });
            } else {
                let mut row = Vec::new();
                for value in line.split_whitespace() {
                    let signed_id: i64 = value
                        .parse()
                        .map_err(|_| AssemblyError::InvalidLine(line_number))?;
                    if signed_id == 0 || signed_id.unsigned_abs() > u32::MAX as u64 {
                        return Err(AssemblyError::InvalidLine(line_number));
                    }
                    row.push(OrientedScaffold {
                        scaffold_id: signed_id.unsigned_abs() as u32,
                        reversed: signed_id < 0,
                    });
                }
                if row.is_empty() {
                    return Err(AssemblyError::InvalidLine(line_number));
                }
                superscaffolds.push(row);
            }
        }

        for row in &superscaffolds {
            for oriented in row {
                if !ids.contains(&oriented.scaffold_id) {
                    return Err(AssemblyError::UnknownScaffold(oriented.scaffold_id));
                }
            }
        }
        if scaffolds.is_empty() {
            return Err(AssemblyError::Empty);
        }
        Ok(Self {
            version: 0,
            scaffolds,
            superscaffolds,
        })
    }

    pub fn total_length(&self) -> u64 {
        self.scaffolds.iter().map(|scaffold| scaffold.length).sum()
    }

    /// Flattens the superscaffold layout into ordered assembly-coordinate
    /// intervals.  The source scaffold definitions stay stable while the
    /// layout can be edited.
    pub fn placements(&self) -> Vec<AssemblyPlacement> {
        let mut lengths = std::collections::HashMap::with_capacity(self.scaffolds.len());
        for scaffold in &self.scaffolds {
            lengths.insert(scaffold.id, scaffold.length);
        }
        let mut start = 0_u64;
        let mut placements = Vec::new();
        for (superscaffold_index, row) in self.superscaffolds.iter().enumerate() {
            for (scaffold_index, oriented) in row.iter().enumerate() {
                // `parse` validates this invariant; retain a defensive skip for
                // documents constructed directly by callers.
                let Some(&length) = lengths.get(&oriented.scaffold_id) else {
                    continue;
                };
                let end = start.saturating_add(length);
                placements.push(AssemblyPlacement {
                    superscaffold_index,
                    scaffold_index,
                    scaffold_id: oriented.scaffold_id,
                    reversed: oriented.reversed,
                    start,
                    end,
                });
                start = end;
            }
        }
        placements
    }

    /// Finds the placed scaffold containing an assembly coordinate.
    pub fn placement_at(&self, coordinate: u64) -> Option<AssemblyPlacement> {
        self.placements()
            .into_iter()
            .find(|placement| placement.start <= coordinate && coordinate < placement.end)
    }

    /// Converts an assembly coordinate to the corresponding offset inside its
    /// original scaffold, applying orientation.
    pub fn source_offset_at(&self, coordinate: u64) -> Option<(AssemblyPlacement, u64)> {
        let placement = self.placement_at(coordinate)?;
        let offset = coordinate - placement.start;
        let source_offset = if placement.reversed {
            placement.end - placement.start - 1 - offset
        } else {
            offset
        };
        Some((placement, source_offset))
    }

    pub fn reverse_superscaffold(&mut self, index: usize) -> Result<(), AssemblyError> {
        let row = self
            .superscaffolds
            .get_mut(index)
            .ok_or(AssemblyError::InvalidSuperscaffold(index))?;
        row.reverse();
        for oriented in row {
            oriented.reversed = !oriented.reversed;
        }
        self.version = self.version.wrapping_add(1);
        Ok(())
    }

    pub fn move_scaffold(
        &mut self,
        from_superscaffold: usize,
        from_index: usize,
        to_superscaffold: usize,
        to_index: usize,
    ) -> Result<(), AssemblyError> {
        let moved = self
            .superscaffolds
            .get(from_superscaffold)
            .ok_or(AssemblyError::InvalidSuperscaffold(from_superscaffold))?
            .get(from_index)
            .copied()
            .ok_or(AssemblyError::InvalidScaffoldPlacement {
                superscaffold: from_superscaffold,
                scaffold: from_index,
            })?;
        let destination_length = self
            .superscaffolds
            .get(to_superscaffold)
            .ok_or(AssemblyError::InvalidSuperscaffold(to_superscaffold))?
            .len();
        let adjusted_index = if from_superscaffold == to_superscaffold && from_index < to_index {
            to_index.saturating_sub(1)
        } else {
            to_index
        };
        let insertion_limit =
            destination_length - usize::from(from_superscaffold == to_superscaffold);
        if adjusted_index > insertion_limit {
            return Err(AssemblyError::InvalidInsertionIndex {
                superscaffold: to_superscaffold,
                index: adjusted_index,
            });
        }
        self.superscaffolds[from_superscaffold].remove(from_index);
        let destination = &mut self.superscaffolds[to_superscaffold];
        destination.insert(adjusted_index, moved);
        self.version = self.version.wrapping_add(1);
        Ok(())
    }
}

/// Snapshot-based editing state.  The document API stays deterministic and
/// UI-free; a later persistent representation can replace snapshots without
/// changing callers.
#[derive(Debug, Clone)]
pub struct AssemblyEditor {
    current: AssemblyDocument,
    undo: Vec<AssemblyDocument>,
    redo: Vec<AssemblyDocument>,
}

impl AssemblyEditor {
    pub fn new(document: AssemblyDocument) -> Self {
        Self {
            current: document,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn document(&self) -> &AssemblyDocument {
        &self.current
    }

    pub fn reverse_superscaffold(&mut self, index: usize) -> Result<(), AssemblyError> {
        self.apply(|document| document.reverse_superscaffold(index))
    }

    pub fn move_scaffold(
        &mut self,
        from_super: usize,
        from_index: usize,
        to_super: usize,
        to_index: usize,
    ) -> Result<(), AssemblyError> {
        self.apply(|document| document.move_scaffold(from_super, from_index, to_super, to_index))
    }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.current.clone());
        self.current = previous;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.current.clone());
        self.current = next;
        true
    }

    fn apply(
        &mut self,
        operation: impl FnOnce(&mut AssemblyDocument) -> Result<(), AssemblyError>,
    ) -> Result<(), AssemblyError> {
        let before = self.current.clone();
        operation(&mut self.current)?;
        self.undo.push(before);
        self.redo.clear();
        Ok(())
    }
}

fn parse_field<T: std::str::FromStr>(
    value: Option<&str>,
    line: usize,
    field: &'static str,
) -> Result<T, AssemblyError> {
    value
        .ok_or(AssemblyError::MissingField { line, field })?
        .parse()
        .map_err(|_| AssemblyError::InvalidLine(line))
}

#[derive(Debug, Error)]
pub enum AssemblyError {
    #[error("I/O error while reading assembly: {0}")]
    Io(#[from] std::io::Error),
    #[error("assembly contains no scaffold definitions")]
    Empty,
    #[error("missing {field} on assembly line {line}")]
    MissingField { line: usize, field: &'static str },
    #[error("invalid assembly syntax on line {0}")]
    InvalidLine(usize),
    #[error("assembly layout references unknown scaffold id {0}")]
    UnknownScaffold(u32),
    #[error("superscaffold {0} does not exist")]
    InvalidSuperscaffold(usize),
    #[error("scaffold position {scaffold} does not exist in superscaffold {superscaffold}")]
    InvalidScaffoldPlacement {
        superscaffold: usize,
        scaffold: usize,
    },
    #[error("insertion index {index} is outside superscaffold {superscaffold}")]
    InvalidInsertionIndex { superscaffold: usize, index: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orientation_and_layout() {
        let document =
            AssemblyDocument::parse(">contig_a 1 1000\n>contig_b 2 2000\n1 -2\n").unwrap();
        assert_eq!(document.total_length(), 3000);
        assert!(!document.superscaffolds[0][0].reversed);
        assert!(document.superscaffolds[0][1].reversed);
    }

    #[test]
    fn maps_coordinates_and_tracks_reverse_and_undo() {
        let document =
            AssemblyDocument::parse(">contig_a 1 10\n>contig_b 2 20\n>contig_c 3 30\n1 -2\n3\n")
                .unwrap();
        assert_eq!(document.source_offset_at(0).unwrap().1, 0);
        assert_eq!(document.source_offset_at(10).unwrap().1, 19);
        assert_eq!(document.source_offset_at(29).unwrap().1, 0);

        let mut editor = AssemblyEditor::new(document);
        editor.reverse_superscaffold(0).unwrap();
        assert_eq!(editor.document().superscaffolds[0][0].scaffold_id, 2);
        assert!(!editor.document().superscaffolds[0][0].reversed);
        assert!(editor.undo());
        assert_eq!(editor.document().superscaffolds[0][0].scaffold_id, 1);
        assert!(editor.redo());
        editor.move_scaffold(1, 0, 0, 1).unwrap();
        assert_eq!(editor.document().superscaffolds[0][1].scaffold_id, 3);
    }

    #[test]
    fn rejected_move_does_not_change_layout() {
        let document = AssemblyDocument::parse(">contig_a 1 10\n>contig_b 2 20\n1\n2\n").unwrap();
        let mut editor = AssemblyEditor::new(document.clone());
        assert!(editor.move_scaffold(0, 0, 1, 2).is_err());
        assert_eq!(editor.document(), &document);
    }
}
