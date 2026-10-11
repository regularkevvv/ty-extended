//! Byte-preserving source projection and sparse diagnostic source maps.
//!
//! Internal edit ranges use UTF-8 byte offsets. CLI and probe positions count
//! Unicode scalar values, as ty GitLab diagnostics do. The editor protocol's
//! negotiated position encoding is handled by `lsp_mapping`.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use crate::model::{Source, Span};

/// Declarations actually emitted by the projection, keyed by full class identity.
/// User-defined constructors and descriptors never enter this registry.
#[derive(Clone, Default)]
pub struct GeneratedDeclarations {
    pub constructors: BTreeSet<String>,
    pub descriptors: BTreeMap<String, BTreeSet<String>>,
}

impl GeneratedDeclarations {
    pub fn extend(&mut self, other: &Self) {
        self.constructors.extend(other.constructors.iter().cloned());
        for (owner, names) in &other.descriptors {
            self.descriptors
                .entry(owner.clone())
                .or_default()
                .extend(names.iter().cloned());
        }
    }
}

#[derive(Default)]
pub struct DiagnosticOrigins {
    pub constructor_calls: BTreeMap<Span, String>,
    pub descriptor_assignments: Vec<DescriptorAssignment>,
    pub virtual_names: BTreeMap<String, String>,
}

pub struct DescriptorAssignment {
    pub value: Span,
    pub target: Span,
    pub owner: String,
    pub member: String,
}

impl DiagnosticOrigins {
    fn normalize(&self, code: &str, original: Span, description: &str) -> (Span, String) {
        let contains = |outer: Span| outer.start <= original.start && original.end <= outer.end;
        if matches!(code, "invalid-argument-type" | "unknown-argument") {
            let constructor = self
                .constructor_calls
                .iter()
                .filter(|(span, _)| contains(**span))
                .min_by_key(|(span, _)| span.end - span.start);
            if let Some((_, owner)) = constructor {
                for name in [owner.as_str(), owner.rsplit('.').next().unwrap_or(owner)] {
                    let printed = format!("{name}.__init__");
                    if code == "invalid-argument-type" {
                        let prefix =
                            format!("invalid-argument-type: Argument to `{printed}` is incorrect");
                        if let Some(detail) = description.strip_prefix(&prefix) {
                            return (
                                original,
                                format!("invalid-argument-type: Argument is incorrect{detail}"),
                            );
                        }
                    } else if let Some(message) =
                        description.strip_suffix(&format!(" of `{printed}`"))
                    {
                        return (original, message.to_owned());
                    }
                }
            }
        }
        if code == "invalid-assignment" {
            let parsed = description
                .strip_prefix(
                    "invalid-assignment: Invalid assignment to data descriptor attribute `",
                )
                .and_then(|rest| rest.split_once("` on type `"))
                .and_then(|(member, rest)| {
                    let (owner, rest) = rest.split_once("`: Expected `")?;
                    let (expected, actual) = rest.split_once("`, found `")?;
                    Some((member, owner, expected, actual.strip_suffix('`')?))
                });
            if let Some((member, owner, expected, actual)) = parsed {
                let mut matches = self.descriptor_assignments.iter().filter(|site| {
                    contains(site.value)
                        && site.member == member
                        && (site.owner == owner || site.owner.rsplit('.').next() == Some(owner))
                });
                if let Some(site) = matches.next() {
                    // A shared RHS in chained assignment can refer to more than
                    // one target. Its diagnostic lacks sufficient identity to
                    // select one target, so retain the checker's own location.
                    if matches.next().is_none() {
                        return (
                            site.target,
                            format!(
                                "invalid-assignment: Object of type `{actual}` is not assignable to attribute `{member}` of type `{expected}`"
                            ),
                        );
                    }
                }
            }
        }
        (original, description.to_owned())
    }
}

#[derive(Clone, Default)]
pub struct Edits {
    operations: Vec<Operation>,
}

#[derive(Clone)]
enum Operation {
    Insert {
        offset: usize,
        text: String,
        origin: Option<Span>,
    },
    Replace {
        span: Span,
        text: String,
    },
    Wrap {
        span: Span,
        before: String,
        after: String,
        probe: Option<String>,
        callable: Option<String>,
    },
    TypeHint {
        span: Span,
        expression: String,
    },
}

impl Edits {
    pub fn insert(&mut self, offset: usize, text: String) {
        self.operations.push(Operation::Insert {
            offset,
            text,
            origin: None,
        });
    }
    /// Anchor a generated declaration to its user-authored declaration. This
    /// allows navigation, but never authorizes applying generated text edits.
    pub fn insert_with_origin(&mut self, offset: usize, text: String, origin: Span) {
        self.operations.push(Operation::Insert {
            offset,
            text,
            origin: Some(origin),
        });
    }
    pub fn replace(&mut self, span: Span, text: String) {
        self.operations.push(Operation::Replace { span, text });
    }
    /// Wrappers are appended from innermost to outermost at identical spans.
    pub fn wrap(&mut self, span: Span, before: String, after: String, probe: Option<String>) {
        self.operations.push(Operation::Wrap {
            span,
            before,
            after,
            probe,
            callable: None,
        });
    }
    pub fn annotate_type(&mut self, span: Span, expression: String) {
        self.operations
            .push(Operation::TypeHint { span, expression });
    }
    pub fn wrap_with_type(
        &mut self,
        span: Span,
        before: String,
        after: String,
        probe: Option<String>,
        expression: String,
    ) {
        self.annotate_type(span, expression);
        self.wrap(span, before, after, probe);
    }
    /// Route callable hover through the actual projected callable selected by
    /// this wrapper, while preserving source navigation through copied bytes.
    pub fn wrap_callable(&mut self, span: Span, before: String, after: String, helper: String) {
        self.operations.push(Operation::Wrap {
            span,
            before,
            after,
            probe: None,
            callable: Some(helper),
        });
    }
    pub fn extend(&mut self, other: Edits) {
        self.operations.extend(other.operations);
    }
}

#[derive(Clone)]
struct Wrapper {
    before: String,
    after: String,
    probe: Option<String>,
    callable: Option<String>,
}

struct Node {
    span: Span,
    replacement: Option<String>,
    wrappers: Vec<Wrapper>,
    children: Vec<usize>,
}
impl Node {
    fn new(span: Span) -> Self {
        Self {
            span,
            replacement: None,
            wrappers: Vec::new(),
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct Mapping {
    generated: Span,
    original: Span,
    exact: bool,
}

#[derive(Clone)]
pub struct Rendered {
    pub text: String,
    pub probes: BTreeMap<(usize, usize), String>,
    mappings: Vec<Mapping>,
    exact_mappings: Vec<usize>,
    lines: Vec<usize>,
    synthetic_casts: BTreeSet<usize>,
    type_hints: Vec<(Span, String)>,
    callable_hovers: Vec<(Span, Span)>,
    synthetic_argument_positions: BTreeSet<usize>,
    signature_masks: Vec<Span>,
}

impl Rendered {
    /// Split the combined notebook projection back into its code cells. Each
    /// source span excludes the one newline separator appended by ty between
    /// cells. Generated preludes belong to the first cell; trailing generated
    /// declarations belong to the last. No offsets are inferred from lines.
    pub fn split_cells(&self, original: &str, cells: &[Span]) -> Result<Vec<Self>> {
        if cells.is_empty() {
            return Ok(Vec::new());
        }
        let mut output = Vec::with_capacity(cells.len());
        let mut expected_original = 0;
        let mut generated_start = 0;
        for (index, cell) in cells.iter().copied().enumerate() {
            ensure!(
                cell.start == expected_original
                    && cell.start <= cell.end
                    && original.get(cell.end..cell.end + 1) == Some("\n"),
                "notebook cells must cover source with one newline separator each"
            );
            ensure!(
                original.is_char_boundary(cell.start) && original.is_char_boundary(cell.end),
                "notebook cell splits a Unicode character"
            );
            let separator = self
                .exact_generated_span(Span {
                    start: cell.end,
                    end: cell.end + 1,
                })
                .context("notebook separator was replaced by a projection")?;
            let final_cell = index + 1 == cells.len();
            let generated_end = if final_cell {
                self.text.len() - usize::from(self.text.ends_with('\n'))
            } else {
                separator.start
            };
            ensure!(
                generated_start <= generated_end,
                "overlapping generated notebook cells"
            );
            output.push(self.slice(
                cell,
                Span {
                    start: generated_start,
                    end: generated_end,
                },
            )?);
            generated_start = separator.end;
            expected_original = cell.end + 1;
        }
        ensure!(
            expected_original == original.len(),
            "notebook cells do not cover combined source"
        );
        Ok(output)
    }

    /// Restrict an existing byte map to one source/generated window. This is
    /// used for native notebook cells; crossing exact chunks are clipped while
    /// unanchored helper declarations remain unmapped.
    pub fn slice(&self, original: Span, generated: Span) -> Result<Self> {
        ensure!(
            original.start <= original.end && generated.start <= generated.end,
            "inverted source-map window"
        );
        let text = self
            .text
            .get(generated.start..generated.end)
            .context("source-map window splits generated UTF-8")?
            .to_owned();
        let shift = |span: Span, offset: usize| Span {
            start: span.start - offset,
            end: span.end - offset,
        };
        let contains =
            |outer: Span, inner: Span| outer.start <= inner.start && inner.end <= outer.end;
        let mut mappings = Vec::new();
        for mapping in &self.mappings {
            let mut start = mapping.generated.start.max(generated.start);
            let mut end = mapping.generated.end.min(generated.end);
            if start >= end {
                continue;
            }
            let mapped_original = if mapping.exact {
                start = start.max(
                    mapping.generated.start + original.start.saturating_sub(mapping.original.start),
                );
                end = end.min(
                    mapping.generated.start + original.end.saturating_sub(mapping.original.start),
                );
                if start >= end {
                    continue;
                }
                Span {
                    start: mapping.original.start + start - mapping.generated.start,
                    end: mapping.original.start + end - mapping.generated.start,
                }
            } else {
                if !contains(original, mapping.original) {
                    continue;
                }
                mapping.original
            };
            mappings.push(Mapping {
                generated: shift(Span { start, end }, generated.start),
                original: shift(mapped_original, original.start),
                exact: mapping.exact,
            });
        }
        let mut lines = vec![0];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' || (byte == b'\r' && text.as_bytes().get(index + 1) != Some(&b'\n')) {
                lines.push(index + 1);
            }
        }
        let exact_mappings = mappings
            .iter()
            .enumerate()
            .filter_map(|(index, mapping)| mapping.exact.then_some(index))
            .collect();
        let points = |values: &BTreeSet<usize>| {
            values
                .range(generated.start..generated.end)
                .map(|offset| offset - generated.start)
                .collect()
        };
        let mut output = Self {
            text,
            lines,
            mappings,
            exact_mappings,
            probes: BTreeMap::new(),
            synthetic_casts: points(&self.synthetic_casts),
            synthetic_argument_positions: points(&self.synthetic_argument_positions),
            type_hints: self
                .type_hints
                .iter()
                .filter(|(span, _)| contains(original, *span))
                .map(|(span, hint)| (shift(*span, original.start), hint.clone()))
                .collect(),
            callable_hovers: self
                .callable_hovers
                .iter()
                .filter(|(source, target)| {
                    contains(original, *source) && contains(generated, *target)
                })
                .map(|(source, target)| {
                    (
                        shift(*source, original.start),
                        shift(*target, generated.start),
                    )
                })
                .collect(),
            signature_masks: self
                .signature_masks
                .iter()
                .filter(|span| contains(generated, **span))
                .map(|span| shift(*span, generated.start))
                .collect(),
        };
        for (&(line, column), key) in &self.probes {
            let offset = offset_at(self, &json!({"line":line,"column":column}))?;
            if generated.start <= offset && offset < generated.end {
                let local = offset - generated.start;
                let line = output
                    .lines
                    .partition_point(|start| *start <= local)
                    .saturating_sub(1);
                let mut column = output.text[output.lines[line]..local].chars().count() + 1;
                if line == 0 && output.text.starts_with('\u{feff}') {
                    column = column.saturating_sub(1).max(1);
                }
                output.probes.insert((line + 1, column), key.clone());
            }
        }
        Ok(output)
    }
    pub fn hover_target(&self, original: usize) -> Option<Span> {
        self.callable_hovers
            .iter()
            .filter(|(span, _)| span.start <= original && original < span.end)
            .min_by_key(|(span, _)| span.end - span.start)
            .map(|(_, generated)| *generated)
    }
    /// A declaration-oriented editor view retains normal calls while removing
    /// only the tuple dispatch to validation helpers. Byte/line positions stay
    /// identical, so the same source map remains valid.
    pub fn signature_text(&self) -> Option<String> {
        if self.signature_masks.is_empty() {
            return None;
        }
        let mut bytes = self.text.as_bytes().to_vec();
        for mask in &self.signature_masks {
            for byte in &mut bytes[mask.start..mask.end] {
                if !matches!(*byte, b'\r' | b'\n') {
                    *byte = b' ';
                }
            }
        }
        String::from_utf8(bytes).ok()
    }
    pub fn hover_type(&self, original: Span) -> Option<&str> {
        // An attribute result refinement applies to its terminal member, never
        // to a receiver or argument contained in the same expression.
        self.type_hints
            .iter()
            .rev()
            .filter(|(span, _)| span.start <= original.start && span.end == original.end)
            .min_by_key(|(span, _)| span.end - span.start)
            .map(|(_, expression)| expression.as_str())
    }
    pub fn synthetic_parameter_hint(&self, generated: usize) -> bool {
        self.synthetic_argument_positions.contains(&generated)
    }
    /// Map a generated read-only range. Synthesized expressions/declarations
    /// return their original enclosing expression/declaration when anchored.
    pub fn original_span(&self, generated: Span) -> Option<Span> {
        map_range(self, generated)
    }

    /// Map edits and semantic tokens only when every byte is copied verbatim.
    /// A diagnostic anchor is deliberately insufficient for applying an edit.
    pub fn exact_original_span(&self, generated: Span) -> Option<Span> {
        self.exact_span(generated, false)
    }

    pub fn exact_generated_span(&self, original: Span) -> Option<Span> {
        self.exact_span(original, true)
    }

    fn exact_span(&self, span: Span, reverse: bool) -> Option<Span> {
        if span.start > span.end {
            return None;
        }
        // Writer emits exact chunks in both source and generated order. The
        // sparse index avoids rescanning a whole module for each semantic token.
        let chunk = |index: usize| {
            let m = &self.mappings[self.exact_mappings[index]];
            if reverse {
                (m.original, m.generated)
            } else {
                (m.generated, m.original)
            }
        };
        let candidate = self
            .exact_mappings
            .partition_point(|index| {
                let m = &self.mappings[*index];
                (if reverse { m.original } else { m.generated }).start <= span.start
            })
            .checked_sub(1)?;
        // Prefer the chunk beginning at a cursor over the chunk ending there.
        // This puts a cursor at the start of a wrapped expression inside it.
        if span.start == span.end {
            let (from, to) = chunk(candidate);
            if span.start > from.end {
                return None;
            }
            let offset = to.start + span.start - from.start;
            return Some(Span {
                start: offset,
                end: offset,
            });
        }
        let mut cursor = span.start;
        let mut target_start = None;
        let mut target_end = 0;
        for index in candidate..self.exact_mappings.len() {
            let (from, to) = chunk(index);
            if from.end <= cursor {
                continue;
            }
            if from.start > cursor {
                break;
            }
            let mapped = to.start + cursor - from.start;
            if target_start.is_some() && mapped != target_end {
                return None;
            }
            target_start.get_or_insert(mapped);
            let end = from.end.min(span.end);
            target_end = to.start + end - from.start;
            cursor = end;
            if cursor == span.end {
                return Some(Span {
                    start: target_start?,
                    end: target_end,
                });
            }
        }
        None
    }

    /// Route a user request into its generated expression. Exact copied text
    /// wins; replacement-only expressions use their enclosing generated span.
    pub fn generated_span(&self, original: Span) -> Option<Span> {
        if let Some(span) = self.exact_generated_span(original) {
            return Some(span);
        }
        if original.start > original.end {
            return None;
        }
        let boundary = |offset: usize, end: bool| {
            let point = Span {
                start: offset,
                end: offset,
            };
            // End positions have left affinity (before a closing wrapper).
            if end {
                if let Some(m) = self
                    .mappings
                    .iter()
                    .filter(|m| m.exact)
                    .find(|m| m.original.start < offset && offset <= m.original.end)
                {
                    return Some(m.generated.start + offset - m.original.start);
                }
            } else if let Some(exact) = self.exact_generated_span(point) {
                return Some(exact.start);
            }
            let m = self
                .mappings
                .iter()
                .filter(|m| m.original.start <= offset && offset <= m.original.end)
                .min_by_key(|m| {
                    (
                        m.original.end - m.original.start,
                        m.generated.end - m.generated.start,
                    )
                })?;
            Some(if end {
                m.generated.end
            } else {
                m.generated.start
            })
        };
        let start = boundary(original.start, false)?;
        let end = if original.start == original.end {
            start
        } else {
            boundary(original.end, true)?
        };
        (start <= end).then_some(Span { start, end })
    }
}

/// Each instrumented module includes an unconditional calibration probe.
/// Checking it catches a missing import, a shadowed non-intrinsic binding, or a
/// checker transport that omitted the file. User probes may all be unreachable;
/// their absence alone cannot distinguish those failures from valid source.
pub fn validate_probe_health(
    source: &Source,
    rendered: &Rendered,
    diagnostics: &[Value],
) -> Result<()> {
    if rendered.probes.is_empty() {
        return Ok(());
    }
    let sentinel = format!("{}probe(None)", source.prefix);
    let lines: Vec<_> = rendered
        .lines
        .iter()
        .enumerate()
        .filter_map(|(index, start)| {
            let end = rendered
                .lines
                .get(index + 1)
                .copied()
                .unwrap_or(rendered.text.len());
            (rendered.text[*start..end].trim_end_matches(['\r', '\n']) == sentinel)
                .then_some(index + 1)
        })
        .collect();
    ensure!(
        lines.len() == 1,
        "{}: expected exactly one generated type-probe calibration",
        source.path
    );
    let verified = diagnostics.iter().filter(|diagnostic| {
        diagnostic["check_name"] == "revealed-type"
            && diagnostic["description"] == "revealed-type: Revealed type: `None`"
            && diagnostic["location"]["positions"]["begin"]["line"].as_u64()
                == Some(lines[0] as u64)
    });
    ensure!(
        verified.count() == 1,
        "{}: generated type probes failed calibration; refusing incomplete plugin inference",
        source.path
    );
    Ok(())
}

fn validate_offset(source: &Source, offset: usize) -> Result<()> {
    ensure!(
        offset <= source.text.len() && source.text.is_char_boundary(offset),
        "edit offset {offset} is outside source or splits a UTF-8 character"
    );
    Ok(())
}

pub fn render(source: &Source, edits: &Edits) -> Result<Rendered> {
    let mut grouped = BTreeMap::<Span, Node>::new();
    let mut insertions = BTreeMap::<usize, Vec<(String, Option<Span>)>>::new();
    let mut type_hints = Vec::new();
    for operation in &edits.operations {
        let span = match operation {
            Operation::TypeHint { span, expression } => {
                validate_offset(source, span.start)?;
                validate_offset(source, span.end)?;
                ensure!(span.start < span.end, "empty/inverted hover type origin");
                type_hints.push((*span, expression.clone()));
                continue;
            }
            Operation::Insert {
                offset,
                text,
                origin,
            } => {
                validate_offset(source, *offset)?;
                if let Some(origin) = origin {
                    validate_offset(source, origin.start)?;
                    validate_offset(source, origin.end)?;
                    ensure!(
                        origin.start <= origin.end,
                        "inverted inserted declaration origin"
                    );
                }
                insertions
                    .entry(*offset)
                    .or_default()
                    .push((text.clone(), *origin));
                continue;
            }
            Operation::Replace { span, .. } | Operation::Wrap { span, .. } => *span,
        };
        validate_offset(source, span.start)?;
        validate_offset(source, span.end)?;
        ensure!(
            span.start < span.end,
            "empty or inverted replacement/wrapper span: {span:?}"
        );
        let node = grouped.entry(span).or_insert_with(|| Node::new(span));
        match operation {
            Operation::Replace { text, .. } => {
                if let Some(previous) = &node.replacement {
                    ensure!(previous == text, "conflicting replacements at {span:?}");
                } else {
                    node.replacement = Some(text.clone());
                }
            }
            Operation::Wrap {
                before,
                after,
                probe,
                callable,
                ..
            } => node.wrappers.push(Wrapper {
                before: before.clone(),
                after: after.clone(),
                probe: probe.clone(),
                callable: callable.clone(),
            }),
            Operation::Insert { .. } | Operation::TypeHint { .. } => unreachable!(),
        }
    }
    let root_span = Span {
        start: 0,
        end: source.text.len(),
    };
    let mut nodes = vec![
        grouped
            .remove(&root_span)
            .unwrap_or_else(|| Node::new(root_span)),
    ];
    let mut ordered: Vec<_> = grouped.into_values().collect();
    ordered.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| right.span.end.cmp(&left.span.end))
    });
    let mut ancestors = vec![0_usize];
    for node in ordered {
        while ancestors.len() > 1 && nodes[*ancestors.last().unwrap()].span.end <= node.span.start {
            ancestors.pop();
        }
        let parent = *ancestors.last().unwrap();
        ensure!(
            node.span.end <= nodes[parent].span.end,
            "crossing edits: {:?} and {:?}",
            nodes[parent].span,
            node.span
        );
        let index = nodes.len();
        nodes[parent].children.push(index);
        nodes.push(node);
        ancestors.push(index);
    }
    for node in &nodes {
        if node.replacement.is_some() {
            ensure!(
                node.children.is_empty(),
                "replacement {:?} contains narrower edits",
                node.span
            );
            ensure!(
                insertions
                    .range((node.span.start + 1)..node.span.end)
                    .next()
                    .is_none(),
                "replacement {:?} contains an insertion",
                node.span
            );
        }
    }
    let mut writer = Writer {
        source,
        nodes: &nodes,
        insertions: insertions.into_iter().collect(),
        next_insertion: 0,
        output: Rendered {
            text: String::with_capacity(source.text.len()),
            probes: BTreeMap::new(),
            mappings: Vec::new(),
            exact_mappings: Vec::new(),
            lines: vec![0],
            synthetic_casts: BTreeSet::new(),
            type_hints,
            callable_hovers: Vec::new(),
            synthetic_argument_positions: BTreeSet::new(),
            signature_masks: Vec::new(),
        },
    };
    writer.node(0)?;
    writer.insert_at(source.text.len())?;
    ensure!(
        writer.next_insertion == writer.insertions.len(),
        "unrendered source insertions"
    );
    writer.output.exact_mappings = writer
        .output
        .mappings
        .iter()
        .enumerate()
        .filter_map(|(index, m)| m.exact.then_some(index))
        .collect();
    Ok(writer.output)
}

type InsertedText = (String, Option<Span>);

struct Writer<'a> {
    source: &'a Source,
    nodes: &'a [Node],
    insertions: Vec<(usize, Vec<InsertedText>)>,
    next_insertion: usize,
    output: Rendered,
}

impl Writer<'_> {
    fn append(&mut self, text: &str) {
        let base = self.output.text.len();
        // An existing trailing CR and an incoming LF are one line ending.
        if self.output.text.ends_with('\r') && text.starts_with('\n') {
            self.output.lines.pop();
        }
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' || (byte == b'\r' && text.as_bytes().get(offset + 1) != Some(&b'\n')) {
                self.output.lines.push(base + offset + 1);
            }
        }
        self.output.text.push_str(text);
    }

    fn insert_at(&mut self, offset: usize) -> Result<()> {
        if let Some((at, _)) = self.insertions.get(self.next_insertion) {
            ensure!(
                *at >= offset,
                "insertion at {at} skipped while rendering offset {offset}"
            );
        }
        if self
            .insertions
            .get(self.next_insertion)
            .is_some_and(|(at, _)| *at == offset)
        {
            // Clone the insertion group to release its borrow before appending.
            let texts = self.insertions[self.next_insertion].1.clone();
            self.next_insertion += 1;
            for (text, origin) in texts {
                let start = self.output.text.len();
                self.append(&text);
                if let Some(original) = origin {
                    self.output.mappings.push(Mapping {
                        generated: Span {
                            start,
                            end: self.output.text.len(),
                        },
                        original,
                        exact: false,
                    });
                }
            }
        }
        Ok(())
    }

    fn original_chunk(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        let generated_start = self.output.text.len();
        self.append(&self.source.text[start..end]);
        self.output.mappings.push(Mapping {
            generated: Span {
                start: generated_start,
                end: self.output.text.len(),
            },
            original: Span { start, end },
            exact: true,
        });
    }

    /// Emit untouched source and insertions strictly before `end`. Insertions
    /// at an expression's start therefore remain outside its opening wrapper.
    fn original(&mut self, mut start: usize, end: usize) -> Result<()> {
        while let Some((at, _)) = self.insertions.get(self.next_insertion) {
            if *at >= end {
                break;
            }
            let at = *at;
            ensure!(
                at >= start,
                "insertion at {at} overlaps already rendered source"
            );
            self.original_chunk(start, at);
            self.insert_at(at)?;
            start = at;
        }
        self.original_chunk(start, end);
        Ok(())
    }

    fn node(&mut self, index: usize) -> Result<()> {
        let node = &self.nodes[index];
        self.insert_at(node.span.start)?;
        let generated_start = self.output.text.len();
        for wrapper in node.wrappers.iter().rev() {
            if wrapper.before.contains(&self.source.cast_name()) {
                self.output.synthetic_casts.insert(self.output.text.len());
            }
            self.append(&wrapper.before);
            if wrapper.before.contains(&self.source.cast_name()) {
                self.output
                    .synthetic_argument_positions
                    .insert(self.output.text.len());
            }
            if let Some(key) = &wrapper.probe {
                let offset = self.output.text.len();
                let line = self.output.lines.len();
                let mut column = self.output.text[self.output.lines[line - 1]..offset]
                    .chars()
                    .count()
                    + 1;
                if line == 1 && self.output.text.starts_with('\u{feff}') {
                    column = column.saturating_sub(1).max(1);
                }
                if let Some(previous) = self.output.probes.insert((line, column), key.clone()) {
                    ensure!(
                        previous == *key,
                        "two distinct probes share generated position {line}:{column}"
                    );
                }
            }
        }
        if let Some(replacement) = &node.replacement {
            let begin = self.output.text.len();
            self.append(replacement);
            self.output.mappings.push(Mapping {
                generated: Span {
                    start: begin,
                    end: self.output.text.len(),
                },
                original: node.span,
                exact: false,
            });
        } else {
            let mut cursor = node.span.start;
            for child in &node.children {
                self.original(cursor, self.nodes[*child].span.start)?;
                self.node(*child)?;
                cursor = self.nodes[*child].span.end;
            }
            self.original(cursor, node.span.end)?;
        }
        for wrapper in &node.wrappers {
            if let Some(helper) = &wrapper.callable {
                let relative = wrapper.after.find(helper).ok_or_else(|| {
                    anyhow!("callable wrapper does not contain its helper reference")
                })?;
                let start = self.output.text.len() + relative;
                self.output.callable_hovers.push((
                    node.span,
                    Span {
                        start,
                        end: start + helper.len(),
                    },
                ));
                let close = wrapper.after[relative + helper.len()..]
                    .find(')')
                    .map(|offset| offset + relative + helper.len())
                    .ok_or_else(|| anyhow!("callable wrapper is missing tuple close"))?;
                let base = self.output.text.len();
                self.output.signature_masks.push(Span {
                    start: base,
                    end: base + close,
                });
                self.output.signature_masks.push(Span {
                    start: base + close + 1,
                    end: base + wrapper.after.len(),
                });
            }
            self.append(&wrapper.after);
        }
        if !node.wrappers.is_empty() {
            self.output.mappings.push(Mapping {
                generated: Span {
                    start: generated_start,
                    end: self.output.text.len(),
                },
                original: node.span,
                exact: false,
            });
        }
        Ok(())
    }
}

fn offset_at(rendered: &Rendered, position: &Value) -> Result<usize> {
    let line = position["line"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing diagnostic line"))? as usize;
    let column = position["column"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing diagnostic column"))? as usize;
    ensure!(
        line > 0 && column > 0,
        "diagnostic positions must be one-based"
    );
    let start = *rendered
        .lines
        .get(line - 1)
        .ok_or_else(|| anyhow!("diagnostic line {line} is outside generated source"))?;
    let start = if line == 1 && rendered.text.starts_with('\u{feff}') {
        start + 3
    } else {
        start
    };
    let end = rendered
        .lines
        .get(line)
        .copied()
        .unwrap_or(rendered.text.len());
    let line_text = &rendered.text[start..end];
    let relative = line_text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(line_text.len()))
        .nth(column - 1)
        .ok_or_else(|| anyhow!("diagnostic position {line}:{column} is outside its line"))?;
    let offset = start + relative;
    Ok(offset)
}

fn map_range(rendered: &Rendered, span: Span) -> Option<Span> {
    let contains = |mapping: &&Mapping| {
        mapping.generated.start <= span.start && span.end <= mapping.generated.end
    };
    // Exact copied bytes have priority over enclosing synthetic expressions.
    if let Some(original) = rendered.exact_original_span(span) {
        return Some(original);
    }
    if let Some(mapping) = rendered
        .mappings
        .iter()
        .filter(contains)
        .min_by_key(|mapping| mapping.generated.end - mapping.generated.start)
    {
        return Some(mapping.original);
    }
    // A diagnostic may cover several copied regions and wrapped expressions.
    // Map each boundary independently, but never invent a location for a
    // diagnostic whose boundaries fall in unanchored declarations/preludes.
    let boundary = |offset: usize, is_end: bool| -> Option<usize> {
        let mapping = rendered
            .mappings
            .iter()
            .filter(|mapping| {
                if is_end {
                    mapping.generated.start < offset && offset <= mapping.generated.end
                } else {
                    mapping.generated.start <= offset && offset < mapping.generated.end
                }
            })
            .min_by_key(|mapping| {
                (
                    !mapping.exact,
                    mapping.generated.end - mapping.generated.start,
                )
            })?;
        Some(if mapping.exact {
            mapping.original.start + offset - mapping.generated.start
        } else if is_end {
            mapping.original.end
        } else {
            mapping.original.start
        })
    };
    let start = boundary(span.start, false)?;
    let end = boundary(span.end, true)?;
    (start <= end).then_some(Span { start, end })
}

pub fn map_diagnostics(
    source: &Source,
    rendered: &Rendered,
    raw: &[Value],
    helpers: &BTreeMap<String, String>,
    plugin_diagnostics: &[Value],
) -> Result<(Vec<Value>, Vec<Value>)> {
    map_diagnostics_with_origins(
        source,
        rendered,
        raw,
        helpers,
        plugin_diagnostics,
        &DiagnosticOrigins::default(),
    )
}

pub fn map_diagnostics_with_origins(
    source: &Source,
    rendered: &Rendered,
    raw: &[Value],
    helpers: &BTreeMap<String, String>,
    plugin_diagnostics: &[Value],
    origins: &DiagnosticOrigins,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut mapped = Vec::new();
    let mut suppressed = Vec::new();
    for diagnostic in raw {
        let positions = &diagnostic["location"]["positions"];
        let begin = offset_at(rendered, &positions["begin"])?;
        let end = offset_at(rendered, &positions["end"])?;
        ensure!(begin <= end, "inverted diagnostic range: {diagnostic}");
        if diagnostic["check_name"] == "redundant-cast" && rendered.synthetic_casts.contains(&begin)
        {
            suppressed.push(diagnostic.clone());
            continue;
        }
        let original = map_range(rendered, Span { start: begin, end })
            .ok_or_else(|| anyhow!("unmapped generated diagnostic: {diagnostic}"))?;
        let mut output = diagnostic.clone();
        let mut description = diagnostic["description"]
            .as_str()
            .ok_or_else(|| anyhow!("diagnostic has no description: {diagnostic}"))?
            .to_owned();
        if diagnostic["check_name"] == "invalid-argument-type" {
            for helper in helpers
                .keys()
                .filter(|name| name.starts_with(&format!("{}call_", source.prefix)))
            {
                let prefix =
                    format!("invalid-argument-type: Argument to function `{helper}` is incorrect");
                if let Some(detail) = description.strip_prefix(&prefix) {
                    description = format!("invalid-argument-type: Argument is incorrect{detail}");
                    break;
                }
            }
        }
        for (helper, original) in helpers {
            description = description.replace(&format!("`{helper}`"), &format!("`{original}`"));
        }
        description = crate::project::rewrite_names(&description, |name| {
            origins.virtual_names.get(name).map_or_else(
                || name.to_owned(),
                |qualified| qualified.rsplit('.').next().unwrap_or(qualified).to_owned(),
            )
        });
        let (original, description) = origins.normalize(
            diagnostic["check_name"].as_str().unwrap_or(""),
            original,
            &description,
        );
        output["location"] = json!({"path":source.path,"positions":{
            "begin":source.position(original.start), "end":source.position(original.end),
        }});
        output["description"] = Value::String(description);
        mapped.push(output);
    }
    for item in plugin_diagnostics {
        let diagnostic = &item["diagnostic"];
        let location = &item["source"];
        let severity = match diagnostic["severity"].as_str() {
            Some("error") => "major",
            Some("warning") => "minor",
            Some("info") => "info",
            _ => bail!(
                "unsupported plugin diagnostic severity: {}",
                diagnostic["severity"]
            ),
        };
        let message = diagnostic["message"]
            .as_str()
            .ok_or_else(|| anyhow!("plugin diagnostic missing message"))?;
        let id = diagnostic["id"]
            .as_str()
            .ok_or_else(|| anyhow!("plugin diagnostic missing id"))?;
        ensure!(
            location["file-path"].is_string()
                && location["start"].is_object()
                && location["end"].is_object(),
            "plugin diagnostic missing source location: {item}"
        );
        mapped.push(json!({
            "check_name":"plugin-configuration", "description":format!("plugin-configuration: {message}: {id}"),
            "severity":severity, "location":{"path":location["file-path"], "positions":{
                "begin":location["start"],"end":location["end"],
            }},
        }));
    }
    Ok((mapped, suppressed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    fn source(text: &str) -> Source {
        Source::new(text.into(), "/original/demo.py".into(), "demo".into())
    }

    #[test]
    fn actual_probe_calibration_rejects_broken_bindings_and_allows_unreachable_probes() -> Result<()>
    {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name official ty")?;
        let source = source("label: str = 42\nif False:\n    value = 10\n");
        let start = source.text.rfind("10").unwrap();
        let analysis = crate::model::Analysis {
            expression_probes: BTreeMap::from([(
                Span {
                    start,
                    end: start + 2,
                },
                "unreachable".into(),
            )]),
            ..Default::default()
        };
        for push in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "ty-native-probe-health-{}-{push}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory)?;
            let mut checker = if push {
                crate::checker::Checker::new_push(std::path::Path::new(&binary), &directory)?
            } else {
                crate::checker::Checker::new(std::path::Path::new(&binary), &directory, false)?
            };
            for import in [
                "typing_extensions import reveal_type",
                "missing_probe_module import reveal_type",
                "typing_extensions import missing_probe_member",
                "builtins import print",
                "typing_extensions import reveal_type",
            ] {
                let mut edits = crate::build_edits(
                    &source,
                    &analysis,
                    Edits::default(),
                    &BTreeMap::new(),
                    true,
                );
                for operation in &mut edits.operations {
                    if let Operation::Insert { text, .. } = operation {
                        *text = text.replace("typing_extensions import reveal_type", import);
                    }
                }
                let rendered = render(&source, &edits)?;
                let diagnostics = checker.check(&directory.join("generated.py"), &rendered.text)?;
                let result = validate_probe_health(&source, &rendered, &diagnostics);
                assert_eq!(
                    result.is_ok(),
                    import == "typing_extensions import reveal_type",
                    "push={push}, import={import}: {result:?}; {diagnostics:?}"
                );
                assert!(
                    diagnostics
                        .iter()
                        .any(|item| item["check_name"] == "invalid-assignment")
                );
                if result.is_ok() {
                    assert_eq!(
                        diagnostics
                            .iter()
                            .filter(|item| item["check_name"] == "revealed-type")
                            .count(),
                        1,
                        "only the calibration should be reachable: {diagnostics:?}"
                    );
                }
            }
            drop(checker);
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    #[test]
    fn generated_constructor_wording_is_scoped_to_proven_source_calls() -> Result<()> {
        let source = source("Projected(value=42)\nOriginal(value=42)\n");
        let rendered = render(&source, &Edits::default())?;
        let first_argument = source.text.find("value=42").unwrap();
        let second_argument = source.text.rfind("value=42").unwrap();
        let message = "invalid-argument-type: Argument to `Record.__init__` is incorrect: Expected `str`, found `Literal[42]`";
        let mut first = diagnostic(
            &rendered,
            "invalid-argument-type",
            first_argument,
            first_argument + 8,
        );
        first["description"] = json!(message);
        let mut second = diagnostic(
            &rendered,
            "invalid-argument-type",
            second_argument,
            second_argument + 8,
        );
        second["description"] = json!(message);
        let origins = DiagnosticOrigins {
            constructor_calls: BTreeMap::from([(
                Span { start: 0, end: 19 },
                "projected.Record".into(),
            )]),
            ..Default::default()
        };
        let (mapped, _) = map_diagnostics_with_origins(
            &source,
            &rendered,
            &[first, second],
            &BTreeMap::new(),
            &[],
            &origins,
        )?;
        assert_eq!(
            mapped[0]["description"],
            "invalid-argument-type: Argument is incorrect: Expected `str`, found `Literal[42]`"
        );
        // A same-named original constructor at another source call is intact.
        assert_eq!(mapped[1]["description"], message);
        assert_eq!(
            mapped[0]["location"]["positions"]["begin"],
            source.position(first_argument)
        );
        let original = Span {
            start: first_argument,
            end: first_argument + 8,
        };
        assert_eq!(origins.normalize("unknown-argument", original, "unknown-argument: Argument `value` does not match any known parameter of `Record.__init__`").1,
            "unknown-argument: Argument `value` does not match any known parameter");
        let unrelated = "invalid-argument-type: Argument to `Different.__init__` is incorrect: Expected `str`, found `int`";
        assert_eq!(
            origins
                .normalize("invalid-argument-type", original, unrelated)
                .1,
            unrelated
        );
        Ok(())
    }

    #[test]
    fn generated_callable_wording_matches_plugin_contract_without_changing_original_functions()
    -> Result<()> {
        let source = source("decode(42)\noriginal(42)\n");
        let rendered = render(&source, &Edits::default())?;
        let first = source.text.find("42").unwrap();
        let second = source.text.rfind("42").unwrap();
        let helper = format!("{}call_0", source.prefix);
        let mut generated = diagnostic(&rendered, "invalid-argument-type", first, first + 2);
        generated["description"] = json!(format!(
            "invalid-argument-type: Argument to function `{helper}` is incorrect: Expected `str`, found `Literal[42]`"
        ));
        let mut original = diagnostic(&rendered, "invalid-argument-type", second, second + 2);
        let original_message = "invalid-argument-type: Argument to function `original` is incorrect: Expected `str`, found `Literal[42]`";
        original["description"] = json!(original_message);
        let (mapped, _) = map_diagnostics(
            &source,
            &rendered,
            &[generated, original],
            &BTreeMap::from([(helper, "demo.decode".into())]),
            &[],
        )?;
        assert_eq!(
            mapped[0]["description"],
            "invalid-argument-type: Argument is incorrect: Expected `str`, found `Literal[42]`"
        );
        assert_eq!(mapped[1]["description"], original_message);
        Ok(())
    }

    #[test]
    fn only_proven_generated_descriptor_moves_error_to_original_target() -> Result<()> {
        let source = source("Record().name = 42\ncustom.name = 42\n");
        let rendered = render(&source, &Edits::default())?;
        let first_value = source.text.find("42").unwrap();
        let second_value = source.text.rfind("42").unwrap();
        let message = "invalid-assignment: Invalid assignment to data descriptor attribute `name` on type `Record`: Expected `str`, found `Literal[42]`";
        let mut first = diagnostic(
            &rendered,
            "invalid-assignment",
            first_value,
            first_value + 2,
        );
        first["description"] = json!(message);
        let mut second = diagnostic(
            &rendered,
            "invalid-assignment",
            second_value,
            second_value + 2,
        );
        second["description"] = json!(message);
        let origins = DiagnosticOrigins {
            descriptor_assignments: vec![DescriptorAssignment {
                value: Span {
                    start: first_value,
                    end: first_value + 2,
                },
                target: Span { start: 0, end: 13 },
                owner: "projected.Record".into(),
                member: "name".into(),
            }],
            ..Default::default()
        };
        let (mapped, _) = map_diagnostics_with_origins(
            &source,
            &rendered,
            &[first, second],
            &BTreeMap::new(),
            &[],
            &origins,
        )?;
        assert_eq!(
            mapped[0]["description"],
            "invalid-assignment: Object of type `Literal[42]` is not assignable to attribute `name` of type `str`"
        );
        assert_eq!(
            mapped[0]["location"]["positions"],
            json!({"begin":{"line":1,"column":1},"end":{"line":1,"column":14}})
        );
        assert_eq!(mapped[1]["description"], message);
        assert_eq!(
            mapped[1]["location"]["positions"]["begin"],
            source.position(second_value)
        );
        Ok(())
    }

    #[test]
    fn virtual_descriptor_names_and_locations_use_registered_origins() -> Result<()> {
        let source = source("record.writable = 1\n");
        let rendered = render(&source, &Edits::default())?;
        let start = source.text.find('1').unwrap();
        let target_end = source.text.find(" =").unwrap();
        let internal = "_generated_virtual_name";
        let origins = DiagnosticOrigins {
            virtual_names: BTreeMap::from([(internal.into(), "virtual.Carrier".into())]),
            descriptor_assignments: vec![DescriptorAssignment {
                value: Span {
                    start,
                    end: start + 1,
                },
                target: Span {
                    start: 0,
                    end: target_end,
                },
                owner: "virtual.Carrier".into(),
                member: "writable".into(),
            }],
            ..Default::default()
        };
        let mut value = diagnostic(&rendered, "invalid-assignment", start, start + 1);
        value["description"] = json!(format!(
            "invalid-assignment: Invalid assignment to data descriptor attribute `writable` on type `{internal}`: Expected `str`, found `Literal[1]`"
        ));
        let (mapped, _) = map_diagnostics_with_origins(
            &source,
            &rendered,
            &[value],
            &BTreeMap::new(),
            &[],
            &origins,
        )?;
        assert_eq!(
            mapped[0]["description"],
            "invalid-assignment: Object of type `Literal[1]` is not assignable to attribute `writable` of type `str`"
        );
        assert_eq!(
            mapped[0]["location"]["positions"]["begin"],
            source.position(0)
        );
        assert_eq!(
            mapped[0]["location"]["positions"]["end"],
            source.position(target_end)
        );
        let mut value = diagnostic(&rendered, "revealed-type", start, start + 1);
        value["description"] = json!(format!(
            "revealed-type: Revealed type: `tuple[type[{internal}], Literal['{internal}']]`"
        ));
        let (mapped, _) = map_diagnostics_with_origins(
            &source,
            &rendered,
            &[value],
            &BTreeMap::new(),
            &[],
            &origins,
        )?;
        assert_eq!(
            mapped[0]["description"],
            format!("revealed-type: Revealed type: `tuple[type[Carrier], Literal['{internal}']]`")
        );
        Ok(())
    }

    #[test]
    fn ambiguous_chained_descriptor_assignment_keeps_checker_location() {
        let source = source("first.name = second.name = 42\n");
        let value = Span { start: 27, end: 29 };
        let origins = DiagnosticOrigins {
            descriptor_assignments: vec![
                DescriptorAssignment {
                    value,
                    target: Span { start: 0, end: 10 },
                    owner: "models.Record".into(),
                    member: "name".into(),
                },
                DescriptorAssignment {
                    value,
                    target: Span { start: 13, end: 24 },
                    owner: "models.Record".into(),
                    member: "name".into(),
                },
            ],
            ..Default::default()
        };
        let message = "invalid-assignment: Invalid assignment to data descriptor attribute `name` on type `Record`: Expected `str`, found `Literal[42]`";
        assert_eq!(
            origins.normalize("invalid-assignment", value, message),
            (value, message.to_owned())
        );
        let _ = source;
    }
    fn diagnostic(rendered: &Rendered, code: &str, start: usize, end: usize) -> Value {
        let position = |offset| {
            let line = rendered.lines.partition_point(|p| *p <= offset) - 1;
            let mut column = rendered.text[rendered.lines[line]..offset].chars().count() + 1;
            if line == 0 && rendered.text.starts_with('\u{feff}') {
                column = column.saturating_sub(1).max(1);
            }
            json!({"line":line + 1,"column":column})
        };
        json!({"check_name":code,"description":"test `helper`","severity":"major",
            "location":{"path":"generated.py","positions":{"begin":position(start),"end":position(end)}}})
    }

    #[test]
    fn nested_wrappers_replacement_probes_and_original_ranges() -> Result<()> {
        let source = source("x = f(42) # keep comment\n");
        let mut edits = Edits::default();
        edits.insert(0, "# prelude\n".into());
        edits.replace(Span { start: 4, end: 5 }, "helper".into());
        edits.wrap(
            Span { start: 6, end: 8 },
            "arg(".into(),
            ")".into(),
            Some("argument".into()),
        );
        edits.wrap(Span { start: 4, end: 9 }, "inner(".into(), ")".into(), None);
        edits.wrap(
            Span { start: 4, end: 9 },
            "outer(".into(),
            ")".into(),
            Some("call".into()),
        );
        let rendered = render(&source, &edits)?;
        assert_eq!(
            rendered.text,
            "# prelude\nx = outer(inner(helper(arg(42)))) # keep comment\n"
        );
        assert_eq!(rendered.probes.get(&(2, 11)), Some(&"call".into()));
        assert_eq!(rendered.probes.get(&(2, 28)), Some(&"argument".into()));
        let number = rendered.text.find("42").unwrap();
        let helper = rendered.text.find("helper").unwrap();
        let outer = rendered.text.find("outer").unwrap();
        let whole_end = rendered.text.find(" # keep").unwrap();
        let raw = vec![
            diagnostic(&rendered, "invalid-argument-type", number, number + 2),
            diagnostic(&rendered, "invalid-call", helper, helper + 6),
            diagnostic(&rendered, "invalid-assignment", outer, whole_end),
        ];
        let helpers = BTreeMap::from([("helper".into(), "f".into())]);
        let (mapped, suppressed) = map_diagnostics(&source, &rendered, &raw, &helpers, &[])?;
        assert!(suppressed.is_empty());
        assert_eq!(
            mapped[0]["location"]["positions"],
            json!({"begin":{"line":1,"column":7},"end":{"line":1,"column":9}})
        );
        assert_eq!(
            mapped[1]["location"]["positions"],
            json!({"begin":{"line":1,"column":5},"end":{"line":1,"column":6}})
        );
        assert_eq!(
            mapped[2]["location"]["positions"],
            json!({"begin":{"line":1,"column":5},"end":{"line":1,"column":10}})
        );
        assert_eq!(mapped[0]["description"], "test `f`");
        Ok(())
    }

    #[test]
    fn utf8_crlf_and_multiline_ranges_preserve_bytes() -> Result<()> {
        let source = source("# café\r\nx = (\r\n    '雪'\r\n)\r\n");
        let begin = source.text.find("'雪'").unwrap();
        let end = begin + "'雪'".len();
        let mut edits = Edits::default();
        edits.insert(0, "# α\n".into());
        edits.wrap(
            Span { start: begin, end },
            "reveal(".into(),
            ")".into(),
            Some("unicode".into()),
        );
        let rendered = render(&source, &edits)?;
        assert!(
            rendered
                .text
                .contains("# café\r\nx = (\r\n    reveal('雪')\r\n)\r\n")
        );
        assert_eq!(rendered.probes.get(&(4, 12)), Some(&"unicode".into()));
        let at = rendered.text.find("'雪'").unwrap();
        let raw = [diagnostic(
            &rendered,
            "invalid-assignment",
            at,
            at + "'雪'".len(),
        )];
        let (mapped, _) = map_diagnostics(&source, &rendered, &raw, &BTreeMap::new(), &[])?;
        assert_eq!(
            mapped[0]["location"]["positions"],
            json!({"begin":{"line":3,"column":5},"end":{"line":3,"column":8}})
        );
        let invalid = json!({"line":4,"column":999}); // outside generated line
        assert!(offset_at(&rendered, &invalid).is_err());
        Ok(())
    }

    #[test]
    fn same_position_insertions_and_empty_source() -> Result<()> {
        let source = source("f()\n");
        let mut edits = Edits::default();
        edits.insert(0, "# first\n".into());
        let mut more = Edits::default();
        more.insert(0, "# second\n".into());
        more.wrap(Span { start: 0, end: 3 }, "w(".into(), ")".into(), None);
        more.insert(3, " # tail".into());
        more.insert(4, "# eof\n".into());
        edits.extend(more);
        assert_eq!(
            render(&source, &edits)?.text,
            "# first\n# second\nw(f()) # tail\n# eof\n"
        );
        let mut empty = Edits::default();
        empty.insert(0, "# empty\n".into());
        assert_eq!(render(&super::tests::source(""), &empty)?.text, "# empty\n");
        Ok(())
    }

    #[test]
    fn conflicts_are_errors() {
        let source = source("abcdef");
        let mut crossing = Edits::default();
        crossing.wrap(Span { start: 0, end: 4 }, "(".into(), ")".into(), None);
        crossing.wrap(Span { start: 2, end: 6 }, "(".into(), ")".into(), None);
        assert!(render(&source, &crossing).is_err());
        let mut contained = Edits::default();
        contained.replace(Span { start: 0, end: 4 }, "x".into());
        contained.wrap(Span { start: 1, end: 3 }, "(".into(), ")".into(), None);
        assert!(render(&source, &contained).is_err());
        let mut duplicate = Edits::default();
        duplicate.replace(Span { start: 0, end: 2 }, "x".into());
        duplicate.replace(Span { start: 0, end: 2 }, "y".into());
        assert!(render(&source, &duplicate).is_err());
        let mut invalid_unicode = Edits::default();
        invalid_unicode.insert(1, "x".into());
        assert!(render(&super::tests::source("雪"), &invalid_unicode).is_err());
    }

    #[test]
    fn only_known_synthetic_redundant_casts_are_suppressed() -> Result<()> {
        let source = source("x = f()\n");
        let mut edits = Edits::default();
        edits.insert(0, "broken_generated = 0\n".into());
        edits.wrap(
            Span { start: 4, end: 7 },
            format!("{}(int, ", source.cast_name()),
            ")".into(),
            None,
        );
        let rendered = render(&source, &edits)?;
        let synthetic = rendered.text.find(&source.cast_name()).unwrap();
        let end = rendered.text.rfind(')').unwrap() + 1;
        let redundant = diagnostic(&rendered, "redundant-cast", synthetic, end);
        let (_, suppressed) =
            map_diagnostics(&source, &rendered, &[redundant], &BTreeMap::new(), &[])?;
        assert_eq!(suppressed.len(), 1);
        let bad = diagnostic(&rendered, "invalid-assignment", 0, 6);
        assert!(map_diagnostics(&source, &rendered, &[bad], &BTreeMap::new(), &[]).is_err());
        let other = diagnostic(&rendered, "invalid-assignment", synthetic, end);
        let (mapped, suppressed) =
            map_diagnostics(&source, &rendered, &[other], &BTreeMap::new(), &[])?;
        assert_eq!(mapped.len(), 1);
        assert!(suppressed.is_empty());
        Ok(())
    }

    #[test]
    fn plugin_diagnostics_keep_their_explicit_locations() -> Result<()> {
        let source = source("x\n");
        let rendered = render(&source, &Edits::default())?;
        let plugin = json!({"diagnostic":{"id":"sample","message":"bad config","severity":"warning"},
            "source":{"file-path":"settings.py","start":{"line":2,"column":3},"end":{"line":2,"column":4}}});
        let (mapped, _) = map_diagnostics(&source, &rendered, &[], &BTreeMap::new(), &[plugin])?;
        assert_eq!(mapped[0]["severity"], "minor");
        assert_eq!(mapped[0]["location"]["path"], "settings.py");
        assert_eq!(
            mapped[0]["description"],
            "plugin-configuration: bad config: sample"
        );
        Ok(())
    }
    #[test]
    fn gitlab_unicode_columns_and_bom_match_official_ty() -> Result<()> {
        let source = source("\u{feff}é = 1; x: str = 42\rprint(x)\n");
        let mut edits = Edits::default();
        let start = source.text.find("42").unwrap();
        edits.wrap(
            Span {
                start,
                end: start + 2,
            },
            "reveal(".into(),
            ")".into(),
            Some("number".into()),
        );
        let rendered = render(&source, &edits)?;
        assert_eq!(rendered.probes.get(&(1, 24)), Some(&"number".into()));
        let generated = rendered.text.find("42").unwrap();
        let raw = [diagnostic(
            &rendered,
            "invalid-assignment",
            generated,
            generated + 2,
        )];
        let (mapped, _) = map_diagnostics(&source, &rendered, &raw, &BTreeMap::new(), &[])?;
        assert_eq!(
            mapped[0]["location"]["positions"],
            json!({"begin":{"line":1,"column":17},"end":{"line":1,"column":19}})
        );
        assert_eq!(
            source.position(source.text.find("print").unwrap()),
            json!({"line":2,"column":1})
        );
        Ok(())
    }

    #[test]
    fn notebook_cells_keep_unicode_offsets_wrappers_and_helpers() -> Result<()> {
        let first = "label = '😀'\r\nvalue = 1";
        let second = "result = value\n";
        let source = source(&format!("{first}\n{second}\n"));
        let mut edits = Edits::default();
        edits.insert(0, "from typing import cast\n".into());
        let original = source.text.rfind("value").unwrap();
        edits.wrap_with_type(
            Span {
                start: original,
                end: original + 5,
            },
            "cast(int, ".into(),
            ")".into(),
            None,
            "int".into(),
        );
        edits.insert(source.text.len(), "final_helper: int = 1\n".into());
        let rendered = render(&source, &edits)?;
        let cells = rendered.split_cells(
            &source.text,
            &[
                Span {
                    start: 0,
                    end: first.len(),
                },
                Span {
                    start: first.len() + 1,
                    end: first.len() + 1 + second.len(),
                },
            ],
        )?;
        assert_eq!(cells[0].text, format!("from typing import cast\n{first}"));
        assert_eq!(
            cells[1].text,
            "result = cast(int, value)\n\nfinal_helper: int = 1"
        );
        let generated = cells[1].text.find("value").unwrap();
        assert_eq!(
            cells[1].exact_original_span(Span {
                start: generated,
                end: generated + 5
            }),
            Some(Span { start: 9, end: 14 })
        );
        assert_eq!(cells[1].hover_type(Span { start: 9, end: 14 }), Some("int"));
        assert!(
            cells[0]
                .exact_original_span(Span { start: 0, end: 4 })
                .is_none()
        );
        Ok(())
    }
}
