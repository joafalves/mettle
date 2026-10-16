//! File-scoped codec registration and conversion-purity checks.

use std::collections::{HashMap, HashSet};

use mettle_capability::media_type::MediaType;
use mettle_syntax::DeclarationKind;

use super::{
    CodecPlan, CompileError, Compiler, Instruction, MettlePlan, NameResolution, PlanExpression,
    PlanExpressionKind, StringPart, qualified_name, resolve_visible_name,
};

impl Compiler<'_> {
    pub(super) fn compile_codecs(&mut self) -> Vec<CodecPlan> {
        let mut ids = HashMap::new();
        let mut media_sets = Vec::new();
        for (id, codec) in self.program.codecs.iter().enumerate() {
            let name = qualified_name(&codec.namespace, &codec.name.value);
            if ids.insert(name.clone(), id).is_some() {
                self.errors.push(CompileError::new(
                    format!("codec `{name}` is declared more than once"),
                    codec.name.span,
                ));
            }
            if self.capability_ids.contains_key(codec.name.value.as_str()) {
                self.errors.push(CompileError::new(
                    format!("codec `{name}` conflicts with a built-in capability"),
                    codec.name.span,
                ));
            }
            media_sets.push(validate_media(codec, &name, &mut self.errors));
        }

        let mut registrations = Vec::new();
        let mut claimed = HashMap::new();
        let mut counts = HashMap::new();
        for registration in &self.program.codec_uses {
            let name = &registration.name.value;
            let id = match resolve_visible_name(
                &ids,
                name,
                &registration.namespace,
                &registration.namespace_uses,
            ) {
                NameResolution::Found(id) => id,
                NameResolution::Missing => {
                    self.errors.push(CompileError::new(
                        format!("codec `{name}` is not defined"),
                        registration.name.span,
                    ));
                    continue;
                }
                NameResolution::Ambiguous(candidates) => {
                    self.errors.push(CompileError::new(
                        format!(
                            "codec `{name}` is ambiguous; use one of {}",
                            candidates.join(", ")
                        ),
                        registration.name.span,
                    ));
                    continue;
                }
            };
            let source = registration.name.span.source;
            let count = counts.entry(source).or_insert(0_usize);
            *count += 1;
            if *count > 64 {
                self.errors.push(CompileError::new(
                    "a file may register at most 64 codecs",
                    registration.name.span,
                ));
            }
            let codec = &self.program.codecs[id];
            for media in &media_sets[id] {
                if let Some(previous) = claimed.insert((source, media.clone()), id) {
                    self.errors.push(CompileError::new(
                        format!(
                            "media type `{media}` is registered more than once in this file ({} and {})",
                            qualified_name(&self.program.codecs[previous].namespace, &self.program.codecs[previous].name.value),
                            qualified_name(&codec.namespace, &codec.name.value)
                        ),
                        registration.name.span,
                    ));
                }
            }
            registrations.push(CodecPlan {
                source,
                name: qualified_name(&codec.namespace, &codec.name.value),
                media_types: media_sets[id].clone(),
                encode_flow: codec.encode_flow,
                decode_flow: codec.decode_flow,
            });
        }
        registrations
    }

    pub(super) fn validate_codec_bodies(&mut self, flows: &[MettlePlan]) {
        for flow in flows {
            if matches!(flow.kind, DeclarationKind::Codec | DeclarationKind::Framing) {
                inspect_statements(
                    &flow.instructions,
                    self.capabilities,
                    &mut self.errors,
                    flow.kind == DeclarationKind::Framing,
                );
            }
        }
    }
}

fn validate_media(
    codec: &mettle_syntax::CodecDecl,
    name: &str,
    errors: &mut Vec<CompileError>,
) -> Vec<String> {
    if codec.aliases.len() > 32 {
        errors.push(CompileError::new(
            "a codec supports at most 32 media-type aliases",
            codec.span,
        ));
    }
    let mut media = Vec::new();
    let mut seen = HashSet::new();
    for item in std::iter::once(&codec.media_type).chain(&codec.aliases) {
        match MediaType::parse(&item.value, item.span) {
            Ok(parsed) => {
                if matches!(
                    parsed.essence.as_str(),
                    "application/json" | "text/plain" | "application/octet-stream"
                ) {
                    errors.push(CompileError::new(
                        "canonical built-in media types cannot be claimed by a custom codec",
                        item.span,
                    ));
                }
                let normalized = parsed.normalized();
                if !seen.insert(normalized.clone()) {
                    errors.push(CompileError::new(
                        format!("codec `{name}` claims `{normalized}` more than once"),
                        item.span,
                    ));
                }
                media.push(normalized);
            }
            Err(error) => errors.push(CompileError::new(error.message, item.span)),
        }
    }
    media
}

fn reject(errors: &mut Vec<CompileError>, span: mettle_syntax::Span) {
    errors.push(CompileError::new(
        "codec and framing bodies must be pure: use values, loops, checks, input.readUntil/readExactly, and built-in json/text/bytes operations only",
        span,
    ));
}

fn inspect_statements(
    instructions: &[Instruction],
    capabilities: &[mettle_capability::CapabilityDescriptor],
    errors: &mut Vec<CompileError>,
    framer: bool,
) {
    for instruction in instructions {
        match instruction {
            Instruction::Break(_) => {}
            Instruction::Echo { value, .. } => {
                reject(errors, value.span);
            }
            Instruction::Yield(value) => inspect_expression(value, capabilities, errors, framer),
            Instruction::If {
                branches,
                else_body,
            } => {
                for branch in branches {
                    inspect_expression(&branch.condition, capabilities, errors, framer);
                    inspect_statements(&branch.instructions, capabilities, errors, framer);
                }
                if let Some(body) = else_body {
                    inspect_statements(body, capabilities, errors, framer);
                }
            }
            Instruction::Bind { expression, .. }
            | Instruction::Evaluate(expression)
            | Instruction::Return(expression) => {
                inspect_expression(expression, capabilities, errors, framer);
            }
            Instruction::Assert { condition, message } => {
                inspect_expression(condition, capabilities, errors, framer);
                if let Some(message) = message {
                    inspect_expression(message, capabilities, errors, framer);
                }
            }
        }
    }
}

fn inspect_expression(
    value: &PlanExpression,
    capabilities: &[mettle_capability::CapabilityDescriptor],
    errors: &mut Vec<CompileError>,
    framer: bool,
) {
    match &value.kind {
        PlanExpressionKind::Constant(_) | PlanExpressionKind::Local(_) => {}
        PlanExpressionKind::Array(values) => {
            for value in values {
                inspect_expression(value, capabilities, errors, framer);
            }
        }
        PlanExpressionKind::Object(fields) => {
            for field in fields {
                inspect_expression(&field.expression, capabilities, errors, framer);
            }
        }
        PlanExpressionKind::Block { instructions, .. } => {
            inspect_statements(instructions, capabilities, errors, framer);
        }
        PlanExpressionKind::Source { instructions, .. } if framer => {
            inspect_statements(instructions, capabilities, errors, framer);
        }
        PlanExpressionKind::For {
            iterable,
            instructions,
            ..
        } => {
            inspect_expression(iterable, capabilities, errors, framer);
            inspect_statements(instructions, capabilities, errors, framer);
        }
        PlanExpressionKind::Member { value, .. }
        | PlanExpressionKind::Not(value)
        | PlanExpressionKind::Negate(value)
        | PlanExpressionKind::Sensitive(value)
        | PlanExpressionKind::TypeOperation { value, .. }
        | PlanExpressionKind::Fail(value) => {
            inspect_expression(value, capabilities, errors, framer);
        }
        PlanExpressionKind::Index { value, index } => {
            inspect_expression(value, capabilities, errors, framer);
            inspect_expression(index, capabilities, errors, framer);
        }
        PlanExpressionKind::Binary { left, right, .. } => {
            inspect_expression(left, capabilities, errors, framer);
            inspect_expression(right, capabilities, errors, framer);
        }
        PlanExpressionKind::InterpolatedString(parts) => {
            for part in parts {
                if let StringPart::Value(value) = part {
                    inspect_expression(value, capabilities, errors, framer);
                }
            }
        }
        PlanExpressionKind::CapabilityCall {
            capability,
            arguments,
            options,
            ..
        } => {
            let name = capabilities.get(*capability).map_or("", |item| item.name);
            if !matches!(name, "json" | "text" | "bytes") {
                reject(errors, value.span);
            }
            for argument in arguments {
                inspect_expression(argument, capabilities, errors, framer);
            }
            for option in options {
                inspect_expression(&option.expression, capabilities, errors, framer);
            }
        }
        PlanExpressionKind::ResourceCall {
            receiver,
            method,
            arguments,
            named_arguments,
        } if framer => {
            if !crate::documentation::is_frame_reader_method(method) {
                reject(errors, value.span);
            }
            inspect_expression(receiver, capabilities, errors, framer);
            for argument in arguments {
                inspect_expression(argument, capabilities, errors, framer);
            }
            for field in named_arguments {
                inspect_expression(&field.expression, capabilities, errors, framer);
            }
        }
        PlanExpressionKind::Context(_)
        | PlanExpressionKind::CodecCall { .. }
        | PlanExpressionKind::FramingCall { .. }
        | PlanExpressionKind::Environment(_)
        | PlanExpressionKind::Source { .. }
        | PlanExpressionKind::ResourceCall { .. }
        | PlanExpressionKind::MettleCall { .. }
        | PlanExpressionKind::Within { .. }
        | PlanExpressionKind::Retry { .. }
        | PlanExpressionKind::Parallel { .. }
        | PlanExpressionKind::Rate { .. }
        | PlanExpressionKind::Concurrency { .. } => reject(errors, value.span),
    }
}
