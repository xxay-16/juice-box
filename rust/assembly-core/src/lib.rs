//! UI-independent parser and domain model for Juicebox `.assembly` files.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::Path,
};

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

/// One scaffold's relationship between the immutable source `.hic` axis and
/// the editable assembly axis. Both intervals are half-open and increasing;
/// `reversed` controls which ends correspond to one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssemblyMappingSegment {
    pub scaffold_id: u32,
    pub source_start: u64,
    pub source_end: u64,
    pub assembly_start: u64,
    pub assembly_end: u64,
    pub reversed: bool,
}

/// Validated bidirectional coordinate index used by heatmap queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyCoordinateMap {
    assembly_segments: Vec<AssemblyMappingSegment>,
    source_segments: Vec<AssemblyMappingSegment>,
    total_length: u64,
}

impl AssemblyCoordinateMap {
    pub fn new(document: &AssemblyDocument) -> Result<Self, AssemblyError> {
        let mut source_by_id = HashMap::with_capacity(document.scaffolds.len());
        let mut source_start = 0_u64;
        for scaffold in &document.scaffolds {
            let source_end = source_start
                .checked_add(scaffold.length)
                .ok_or(AssemblyError::CoordinateOverflow)?;
            source_by_id.insert(scaffold.id, (source_start, source_end));
            source_start = source_end;
        }

        let mut seen = HashSet::with_capacity(document.scaffolds.len());
        let mut assembly_start = 0_u64;
        let mut assembly_segments = Vec::with_capacity(document.scaffolds.len());
        for row in &document.superscaffolds {
            for oriented in row {
                if !seen.insert(oriented.scaffold_id) {
                    return Err(AssemblyError::DuplicatePlacement(oriented.scaffold_id));
                }
                let &(segment_source_start, segment_source_end) = source_by_id
                    .get(&oriented.scaffold_id)
                    .ok_or(AssemblyError::UnknownScaffold(oriented.scaffold_id))?;
                let length = segment_source_end - segment_source_start;
                let assembly_end = assembly_start
                    .checked_add(length)
                    .ok_or(AssemblyError::CoordinateOverflow)?;
                assembly_segments.push(AssemblyMappingSegment {
                    scaffold_id: oriented.scaffold_id,
                    source_start: segment_source_start,
                    source_end: segment_source_end,
                    assembly_start,
                    assembly_end,
                    reversed: oriented.reversed,
                });
                assembly_start = assembly_end;
            }
        }
        if let Some(scaffold) = document
            .scaffolds
            .iter()
            .find(|scaffold| !seen.contains(&scaffold.id))
        {
            return Err(AssemblyError::MissingPlacement(scaffold.id));
        }

        let mut source_segments = assembly_segments.clone();
        source_segments.sort_unstable_by_key(|segment| segment.source_start);
        Ok(Self {
            assembly_segments,
            source_segments,
            total_length: assembly_start,
        })
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub fn segments(&self) -> &[AssemblyMappingSegment] {
        &self.assembly_segments
    }

    pub fn assembly_to_source(&self, coordinate: u64) -> Option<u64> {
        let segment = find_by_assembly(&self.assembly_segments, coordinate)?;
        let offset = coordinate - segment.assembly_start;
        Some(if segment.reversed {
            segment.source_end - 1 - offset
        } else {
            segment.source_start + offset
        })
    }

    pub fn source_to_assembly(&self, coordinate: u64) -> Option<u64> {
        let segment = find_by_source(&self.source_segments, coordinate)?;
        let offset = coordinate - segment.source_start;
        Some(if segment.reversed {
            segment.assembly_end - 1 - offset
        } else {
            segment.assembly_start + offset
        })
    }

    /// Maps a source `.hic` bin to the assembly bin containing the same
    /// interval. This mirrors Juicebox's bin-start convention and accounts for
    /// the bin width when a scaffold is reversed.
    pub fn source_bin_to_assembly_bin(&self, source_bin: i32, bin_size: u32) -> Option<i32> {
        if source_bin < 0 || bin_size == 0 {
            return None;
        }
        let bin_size = u64::from(bin_size);
        let source_start = u64::try_from(source_bin).ok()?.checked_mul(bin_size)?;
        let segment = find_by_source(&self.source_segments, source_start)?;
        let assembly_start = if segment.reversed {
            let source_offset = source_start.saturating_sub(segment.source_start);
            segment
                .assembly_end
                .saturating_sub(source_offset.saturating_add(bin_size))
        } else {
            segment
                .assembly_start
                .saturating_add(source_start - segment.source_start)
        };
        i32::try_from(assembly_start / bin_size).ok()
    }

    /// Returns clipped scaffold segments intersecting an assembly-axis range.
    /// Source interval bounds remain increasing even for reversed segments.
    pub fn segments_for_assembly_range(&self, start: u64, end: u64) -> Vec<AssemblyMappingSegment> {
        if start >= end || start >= self.total_length {
            return Vec::new();
        }
        let end = end.min(self.total_length);
        self.assembly_segments
            .iter()
            .filter_map(|segment| {
                let assembly_start = start.max(segment.assembly_start);
                let assembly_end = end.min(segment.assembly_end);
                if assembly_start >= assembly_end {
                    return None;
                }
                let (source_start, source_end) = if segment.reversed {
                    (
                        segment.source_end - (assembly_end - segment.assembly_start),
                        segment.source_end - (assembly_start - segment.assembly_start),
                    )
                } else {
                    (
                        segment.source_start + (assembly_start - segment.assembly_start),
                        segment.source_start + (assembly_end - segment.assembly_start),
                    )
                };
                Some(AssemblyMappingSegment {
                    scaffold_id: segment.scaffold_id,
                    source_start,
                    source_end,
                    assembly_start,
                    assembly_end,
                    reversed: segment.reversed,
                })
            })
            .collect()
    }
}

fn find_by_assembly(
    segments: &[AssemblyMappingSegment],
    coordinate: u64,
) -> Option<&AssemblyMappingSegment> {
    let index = segments.partition_point(|segment| segment.assembly_start <= coordinate);
    let segment = segments.get(index.checked_sub(1)?)?;
    (coordinate < segment.assembly_end).then_some(segment)
}

fn find_by_source(
    segments: &[AssemblyMappingSegment],
    coordinate: u64,
) -> Option<&AssemblyMappingSegment> {
    let index = segments.partition_point(|segment| segment.source_start <= coordinate);
    let segment = segments.get(index.checked_sub(1)?)?;
    (coordinate < segment.source_end).then_some(segment)
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

    pub fn coordinate_map(&self) -> Result<AssemblyCoordinateMap, AssemblyError> {
        AssemblyCoordinateMap::new(self)
    }

    pub fn to_assembly_text(&self) -> String {
        let mut output = String::new();
        for scaffold in &self.scaffolds {
            writeln!(
                output,
                ">{} {} {}",
                scaffold.name, scaffold.id, scaffold.length
            )
            .expect("writing to a String cannot fail");
        }
        for row in &self.superscaffolds {
            for (index, oriented) in row.iter().enumerate() {
                if index > 0 {
                    output.push(' ');
                }
                if oriented.reversed {
                    output.push('-');
                }
                write!(output, "{}", oriented.scaffold_id)
                    .expect("writing to a String cannot fail");
            }
            output.push('\n');
        }
        output
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), AssemblyError> {
        fs::write(path, self.to_assembly_text())?;
        Ok(())
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

    pub fn toggle_scaffold_orientation(
        &mut self,
        superscaffold: usize,
        scaffold: usize,
    ) -> Result<(), AssemblyError> {
        let oriented = self
            .superscaffolds
            .get_mut(superscaffold)
            .ok_or(AssemblyError::InvalidSuperscaffold(superscaffold))?
            .get_mut(scaffold)
            .ok_or(AssemblyError::InvalidScaffoldPlacement {
                superscaffold,
                scaffold,
            })?;
        oriented.reversed = !oriented.reversed;
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

    /// Split one scaffold into source-order fragments and move the selected
    /// middle interval to its own debris superscaffold. The two retained
    /// fragments stay adjacent in the original superscaffold, respecting the
    /// current orientation.
    pub fn extract_debris(
        &mut self,
        superscaffold: usize,
        scaffold_index: usize,
        start_cut: u64,
        end_cut: u64,
    ) -> Result<[u32; 3], AssemblyError> {
        let oriented = *self
            .superscaffolds
            .get(superscaffold)
            .ok_or(AssemblyError::InvalidSuperscaffold(superscaffold))?
            .get(scaffold_index)
            .ok_or(AssemblyError::InvalidScaffoldPlacement {
                superscaffold,
                scaffold: scaffold_index,
            })?;
        let definition_index = self
            .scaffolds
            .iter()
            .position(|scaffold| scaffold.id == oriented.scaffold_id)
            .ok_or(AssemblyError::UnknownScaffold(oriented.scaffold_id))?;
        let original = self.scaffolds[definition_index].clone();
        if start_cut == 0 || start_cut >= end_cut || end_cut >= original.length {
            return Err(AssemblyError::InvalidCutRange {
                scaffold: original.id,
                start: start_cut,
                end: end_cut,
                length: original.length,
            });
        }
        if original.name.contains(":::debris") {
            return Err(AssemblyError::CannotSplitDebris(original.id));
        }
        // Java Juicebox keeps scaffold ids identical to their 1-based cprops
        // order. Shift all following ids by two before inserting the three
        // fragments so the saved file remains consumable by the Java editor.
        for scaffold in &mut self.scaffolds {
            if scaffold.id > original.id {
                scaffold.id = scaffold
                    .id
                    .checked_add(2)
                    .ok_or(AssemblyError::CoordinateOverflow)?;
            }
        }
        for row in &mut self.superscaffolds {
            for placement in row {
                if placement.scaffold_id > original.id {
                    placement.scaffold_id = placement
                        .scaffold_id
                        .checked_add(2)
                        .ok_or(AssemblyError::CoordinateOverflow)?;
                }
            }
        }
        let next_id = original
            .id
            .checked_add(1)
            .ok_or(AssemblyError::CoordinateOverflow)?;
        let right_id = original
            .id
            .checked_add(2)
            .ok_or(AssemblyError::CoordinateOverflow)?;
        let base_name = original
            .name
            .split(":::fragment_")
            .next()
            .unwrap_or(&original.name);
        let left = Scaffold {
            name: format!("{base_name}:::fragment_1"),
            id: original.id,
            length: start_cut,
        };
        let debris = Scaffold {
            name: format!("{base_name}:::fragment_2:::debris"),
            id: next_id,
            length: end_cut - start_cut,
        };
        let right = Scaffold {
            name: format!("{base_name}:::fragment_3"),
            id: right_id,
            length: original.length - end_cut,
        };
        self.scaffolds
            .splice(definition_index..=definition_index, [left, debris, right]);

        let retained = if oriented.reversed {
            vec![
                OrientedScaffold {
                    scaffold_id: right_id,
                    reversed: true,
                },
                OrientedScaffold {
                    scaffold_id: original.id,
                    reversed: true,
                },
            ]
        } else {
            vec![
                OrientedScaffold {
                    scaffold_id: original.id,
                    reversed: false,
                },
                OrientedScaffold {
                    scaffold_id: right_id,
                    reversed: false,
                },
            ]
        };
        self.superscaffolds[superscaffold].splice(scaffold_index..=scaffold_index, retained);
        self.superscaffolds.push(vec![OrientedScaffold {
            scaffold_id: next_id,
            reversed: oriented.reversed,
        }]);
        self.version = self.version.wrapping_add(1);
        Ok([original.id, next_id, right_id])
    }

    pub fn split_superscaffold_after(
        &mut self,
        superscaffold: usize,
        scaffold_index: usize,
    ) -> Result<(), AssemblyError> {
        let row = self
            .superscaffolds
            .get_mut(superscaffold)
            .ok_or(AssemblyError::InvalidSuperscaffold(superscaffold))?;
        if scaffold_index + 1 >= row.len() {
            return Err(AssemblyError::InvalidGroupBoundary {
                superscaffold,
                scaffold: scaffold_index,
            });
        }
        let tail = row.split_off(scaffold_index + 1);
        self.superscaffolds.insert(superscaffold + 1, tail);
        self.version = self.version.wrapping_add(1);
        Ok(())
    }

    pub fn merge_superscaffold_with_next(
        &mut self,
        superscaffold: usize,
    ) -> Result<(), AssemblyError> {
        if superscaffold + 1 >= self.superscaffolds.len() {
            return Err(AssemblyError::InvalidMergeBoundary(superscaffold));
        }
        let next = self.superscaffolds.remove(superscaffold + 1);
        self.superscaffolds[superscaffold].extend(next);
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

    pub fn toggle_scaffold_orientation(
        &mut self,
        superscaffold: usize,
        scaffold: usize,
    ) -> Result<(), AssemblyError> {
        self.apply(|document| document.toggle_scaffold_orientation(superscaffold, scaffold))
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

    pub fn extract_debris(
        &mut self,
        superscaffold: usize,
        scaffold: usize,
        start_cut: u64,
        end_cut: u64,
    ) -> Result<[u32; 3], AssemblyError> {
        let mut ids = [0; 3];
        self.apply(|document| {
            ids = document.extract_debris(superscaffold, scaffold, start_cut, end_cut)?;
            Ok(())
        })?;
        Ok(ids)
    }

    pub fn split_superscaffold_after(
        &mut self,
        superscaffold: usize,
        scaffold: usize,
    ) -> Result<(), AssemblyError> {
        self.apply(|document| document.split_superscaffold_after(superscaffold, scaffold))
    }

    pub fn merge_superscaffold_with_next(
        &mut self,
        superscaffold: usize,
    ) -> Result<(), AssemblyError> {
        self.apply(|document| document.merge_superscaffold_with_next(superscaffold))
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
    #[error("assembly layout places scaffold id {0} more than once")]
    DuplicatePlacement(u32),
    #[error("assembly layout does not place scaffold id {0}")]
    MissingPlacement(u32),
    #[error("assembly coordinates exceed the supported 64-bit range")]
    CoordinateOverflow,
    #[error("superscaffold {0} does not exist")]
    InvalidSuperscaffold(usize),
    #[error("scaffold position {scaffold} does not exist in superscaffold {superscaffold}")]
    InvalidScaffoldPlacement {
        superscaffold: usize,
        scaffold: usize,
    },
    #[error("insertion index {index} is outside superscaffold {superscaffold}")]
    InvalidInsertionIndex { superscaffold: usize, index: usize },
    #[error("invalid cut [{start}, {end}) for scaffold {scaffold} of length {length}")]
    InvalidCutRange {
        scaffold: u32,
        start: u64,
        end: u64,
        length: u64,
    },
    #[error("debris scaffold {0} cannot be split again")]
    CannotSplitDebris(u32),
    #[error("cannot split after scaffold {scaffold} in superscaffold {superscaffold}")]
    InvalidGroupBoundary {
        superscaffold: usize,
        scaffold: usize,
    },
    #[error("superscaffold {0} has no following group to merge")]
    InvalidMergeBoundary(usize),
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

    #[test]
    fn extracts_debris_and_preserves_coordinate_coverage() {
        let document = AssemblyDocument::parse(">a 1 100\n>b 2 20\n>c 3 30\n1 2\n3\n").unwrap();
        let mut editor = AssemblyEditor::new(document);
        assert_eq!(editor.extract_debris(0, 0, 20, 35).unwrap(), [1, 2, 3]);
        let document = editor.document();
        assert_eq!(document.total_length(), 150);
        assert_eq!(
            document.scaffolds.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(document.superscaffolds.len(), 3);
        assert_eq!(
            document.superscaffolds[0]
                .iter()
                .map(|placed| placed.scaffold_id)
                .collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(document.superscaffolds[2][0].scaffold_id, 2);
        assert!(document.scaffolds[1].name.ends_with(":::debris"));
        assert_eq!(document.coordinate_map().unwrap().total_length(), 150);
        assert!(editor.undo());
        assert_eq!(editor.document().scaffolds.len(), 3);
        assert!(editor.redo());
        assert_eq!(editor.document().scaffolds.len(), 5);
        AssemblyDocument::parse(&editor.document().to_assembly_text()).unwrap();
    }

    #[test]
    fn reversed_debris_split_keeps_retained_fragments_in_display_order() {
        let mut document = AssemblyDocument::parse(">a 1 100\n-1\n").unwrap();
        document.extract_debris(0, 0, 20, 35).unwrap();
        assert_eq!(
            document.superscaffolds[0]
                .iter()
                .map(|placed| (placed.scaffold_id, placed.reversed))
                .collect::<Vec<_>>(),
            vec![(3, true), (1, true)]
        );
        assert_eq!(
            document.superscaffolds[1],
            vec![OrientedScaffold {
                scaffold_id: 2,
                reversed: true
            }]
        );
        assert_eq!(document.coordinate_map().unwrap().total_length(), 100);
    }

    #[test]
    fn splits_and_merges_superscaffold_with_undo() {
        let document = AssemblyDocument::parse(">a 1 10\n>b 2 20\n>c 3 30\n1 2 3\n").unwrap();
        let mut editor = AssemblyEditor::new(document);
        editor.split_superscaffold_after(0, 0).unwrap();
        assert_eq!(editor.document().superscaffolds.len(), 2);
        assert_eq!(editor.document().superscaffolds[0].len(), 1);
        editor.merge_superscaffold_with_next(0).unwrap();
        assert_eq!(editor.document().superscaffolds[0].len(), 3);
        assert!(editor.undo());
        assert_eq!(editor.document().superscaffolds.len(), 2);
    }

    #[test]
    fn coordinate_map_tracks_reorder_and_orientation() {
        let document = AssemblyDocument::parse(">a 1 10\n>b 2 20\n>c 3 5\n-2 3\n1\n").unwrap();
        let map = document.coordinate_map().unwrap();
        assert_eq!(map.total_length(), 35);
        assert_eq!(map.assembly_to_source(0), Some(29));
        assert_eq!(map.assembly_to_source(19), Some(10));
        assert_eq!(map.assembly_to_source(20), Some(30));
        assert_eq!(map.assembly_to_source(25), Some(0));
        assert_eq!(map.source_to_assembly(10), Some(19));
        assert_eq!(map.source_to_assembly(29), Some(0));
        assert_eq!(map.source_to_assembly(34), Some(24));
        assert_eq!(map.source_to_assembly(35), None);
        assert_eq!(map.source_bin_to_assembly_bin(1, 5), Some(6));
        assert_eq!(map.source_bin_to_assembly_bin(2, 5), Some(3));
        assert_eq!(map.source_bin_to_assembly_bin(3, 5), Some(2));
        assert_eq!(map.source_bin_to_assembly_bin(6, 5), Some(4));

        assert_eq!(
            map.segments_for_assembly_range(5, 23),
            vec![
                AssemblyMappingSegment {
                    scaffold_id: 2,
                    source_start: 10,
                    source_end: 25,
                    assembly_start: 5,
                    assembly_end: 20,
                    reversed: true,
                },
                AssemblyMappingSegment {
                    scaffold_id: 3,
                    source_start: 30,
                    source_end: 33,
                    assembly_start: 20,
                    assembly_end: 23,
                    reversed: false,
                },
            ]
        );
    }

    #[test]
    fn coordinate_map_rejects_duplicate_and_missing_placements() {
        let duplicate = AssemblyDocument::parse(">a 1 10\n>b 2 20\n1 1 2\n").unwrap();
        assert!(matches!(
            duplicate.coordinate_map(),
            Err(AssemblyError::DuplicatePlacement(1))
        ));
        let missing = AssemblyDocument::parse(">a 1 10\n>b 2 20\n1\n").unwrap();
        assert!(matches!(
            missing.coordinate_map(),
            Err(AssemblyError::MissingPlacement(2))
        ));
    }

    #[test]
    fn saves_round_trippable_assembly_and_undoes_orientation() {
        let document = AssemblyDocument::parse(">a 1 10\n>b 2 20\n1 -2\n").unwrap();
        assert_eq!(
            AssemblyDocument::parse(&document.to_assembly_text()).unwrap(),
            document
        );
        let mut editor = AssemblyEditor::new(document);
        editor.toggle_scaffold_orientation(0, 0).unwrap();
        assert!(editor.document().superscaffolds[0][0].reversed);
        assert!(editor.undo());
        assert!(!editor.document().superscaffolds[0][0].reversed);
    }
}
