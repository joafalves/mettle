//! Framing declarations: built-in validation against shared bounds, and
//! lowering of `name.frames(...)` / `name.encode(...)` calls.

use std::collections::{HashMap, HashSet};

use mettle_capability::framing::{
    BuiltinFraming, DEFAULT_MAX_FRAME_BYTES, LengthPrefix, MAX_FRAME_BYTES_LIMIT,
};
use mettle_capability::media_type::MediaType;
use mettle_syntax::{Expression, ExpressionKind, FramerDecl, ObjectField, Span, Spanned};

use super::{
    CompileError, Compiler, FramerPlan, NameResolution, PlanExpression, PlanExpressionKind,
    ValueType, qualified_name, resolve_visible_name,
};
use crate::FramingKind;
use crate::documentation::{self, FramingOption};
use mettle_syntax::language::Item;

/// Validated named arguments of a built-in framing.
#[derive(Default)]
struct FramingOptions {
    media_type: Option<String>,
    max_frame_bytes: Option<(usize, Span)>,
    require_final_delimiter: Option<bool>,
}

impl Compiler<'_> {
    pub(super) fn compile_framers(&mut self) -> Vec<FramerPlan> {
        let mut names = HashSet::new();
        let mut plans = Vec::with_capacity(self.program.framers.len());
        for framer in &self.program.framers {
            let name = qualified_name(&framer.namespace, &framer.name.value);
            if !names.insert(name.clone()) {
                self.errors.push(CompileError::new(
                    format!("framing `{name}` is declared more than once"),
                    framer.name.span,
                ));
            }
            if self
                .program
                .codecs
                .iter()
                .any(|codec| qualified_name(&codec.namespace, &codec.name.value) == name)
            {
                self.errors.push(CompileError::new(
                    format!("framing `{name}` conflicts with a codec of the same name"),
                    framer.name.span,
                ));
            }
            if self.capability_ids.contains_key(framer.name.value.as_str()) {
                self.errors.push(CompileError::new(
                    format!("framing `{name}` conflicts with a built-in capability"),
                    framer.name.span,
                ));
            }
            // Plans stay index-aligned with declarations; errors fail compilation.
            let (framing, media_type, max_frame_bytes) = if let Some(builtin) = &framer.builtin {
                self.compile_builtin_framing(framer, builtin)
            } else {
                let max = framer
                    .max_frame_bytes
                    .as_ref()
                    .and_then(|value| self.frame_size(value, "maxFrameBytes"))
                    .unwrap_or(DEFAULT_MAX_FRAME_BYTES);
                let custom = FramingKind::Custom {
                    read_flow: framer.read_flow.expect("custom framing read method"),
                    write_flow: framer.write_flow.expect("custom framing write method"),
                };
                (custom, None, max)
            };
            plans.push(FramerPlan {
                name,
                framing,
                media_type,
                max_frame_bytes,
            });
        }
        plans
    }

    fn compile_builtin_framing(
        &mut self,
        framer: &FramerDecl,
        builtin: &Expression,
    ) -> (FramingKind, Option<String>, usize) {
        let fallback = (
            FramingKind::Builtin(BuiltinFraming::Fixed(1)),
            None,
            DEFAULT_MAX_FRAME_BYTES,
        );
        // The parser suppresses trailing option blocks after a framing.
        let ExpressionKind::Call {
            callee,
            arguments,
            named_arguments,
            ..
        } = &builtin.kind
        else {
            return fallback;
        };
        let Some(item) = documentation::framing(&callee.value) else {
            let names = documentation::FRAMINGS
                .iter()
                .map(|item| format!("`{}`", item.name))
                .collect::<Vec<_>>()
                .join(", ");
            self.errors.push(CompileError::new(
                format!(
                    "unknown built-in framing `{}`; expected one of {names}",
                    callee.value
                ),
                callee.span,
            ));
            return fallback;
        };
        if arguments.len() != 1 {
            self.errors.push(CompileError::new(
                format!("`{}` takes exactly one positional argument", item.form),
                builtin.span,
            ));
            return fallback;
        }
        let options = self.framing_options(framer, item, named_arguments);
        let Some(framing) = self.builtin_framing(item, &arguments[0], &options) else {
            return fallback;
        };
        let (max_frame_bytes, max_span) = match (&framing, options.max_frame_bytes) {
            (BuiltinFraming::Fixed(size), _) => (*size, builtin.span),
            (_, Some(explicit)) => explicit,
            (_, None) => (DEFAULT_MAX_FRAME_BYTES, builtin.span),
        };
        let overhead = match &framing {
            BuiltinFraming::Delimited { delimiter, .. } => delimiter.len(),
            BuiltinFraming::LengthPrefixed(prefix) => prefix.width(),
            BuiltinFraming::Fixed(_) => 0,
        };
        if overhead >= max_frame_bytes {
            self.errors.push(CompileError::new(
                format!(
                    "maxFrameBytes ({max_frame_bytes}) leaves no room for a body after the {overhead}-byte boundary"
                ),
                max_span,
            ));
        }
        (
            FramingKind::Builtin(framing),
            options.media_type,
            max_frame_bytes,
        )
    }

    fn framing_options(
        &mut self,
        framer: &FramerDecl,
        item: &Item,
        named_arguments: &[ObjectField],
    ) -> FramingOptions {
        let mut options = FramingOptions::default();
        for field in named_arguments {
            match documentation::framing_option(item.name, &field.name.value) {
                Some(FramingOption::MediaType) => {
                    options.media_type = self.framing_media_type(framer, field);
                }
                Some(FramingOption::MaxFrameBytes) => {
                    options.max_frame_bytes = self
                        .frame_size(&field.expression, "maxFrameBytes")
                        .map(|size| (size, field.span));
                }
                Some(FramingOption::RequireFinalDelimiter) => {
                    if let ExpressionKind::Boolean(value) = field.expression.kind {
                        options.require_final_delimiter = Some(value);
                    } else {
                        self.errors.push(CompileError::new(
                            "requireFinalDelimiter must be true or false",
                            field.expression.span,
                        ));
                    }
                }
                None => self.errors.push(CompileError::new(
                    format!(
                        "`{}` does not accept `{}`; see `mettle docs framing.{}`",
                        item.name, field.name.value, item.name
                    ),
                    field.name.span,
                )),
            }
        }
        options
    }

    fn builtin_framing(
        &mut self,
        item: &Item,
        argument: &Expression,
        options: &FramingOptions,
    ) -> Option<BuiltinFraming> {
        match item.name {
            "delimited" => match &argument.kind {
                ExpressionKind::String(delimiter) if !delimiter.is_empty() => {
                    Some(BuiltinFraming::Delimited {
                        delimiter: delimiter.clone(),
                        require_final_delimiter: options.require_final_delimiter.unwrap_or(true),
                    })
                }
                _ => {
                    self.errors.push(CompileError::new(
                        "delimited framing requires a nonempty string literal delimiter",
                        argument.span,
                    ));
                    None
                }
            },
            "lengthPrefixed" => {
                let prefix = match &argument.kind {
                    ExpressionKind::Name(name) => LengthPrefix::parse(name),
                    _ => None,
                };
                if prefix.is_none() {
                    let names = LengthPrefix::ALL
                        .iter()
                        .map(|prefix| prefix.name())
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.errors.push(CompileError::new(
                        format!("length prefix must be one of {names}"),
                        argument.span,
                    ));
                }
                prefix.map(BuiltinFraming::LengthPrefixed)
            }
            _ => self
                .frame_size(argument, "fixed frame size")
                .map(BuiltinFraming::Fixed),
        }
    }

    fn frame_size(&mut self, value: &Expression, what: &str) -> Option<usize> {
        let size = match value.kind {
            ExpressionKind::Integer(size) => usize::try_from(size).ok(),
            _ => None,
        }
        .filter(|size| (1..=MAX_FRAME_BYTES_LIMIT).contains(size));
        if size.is_none() {
            self.errors.push(CompileError::new(
                format!(
                    "{what} must be a positive integer literal no greater than {MAX_FRAME_BYTES_LIMIT}"
                ),
                value.span,
            ));
        }
        size
    }

    /// Resolve a literal, built-in `json.mediaType`-style constant, or a
    /// declared codec's `name.mediaType` to one normalized media type.
    fn framing_media_type(&mut self, framer: &FramerDecl, field: &ObjectField) -> Option<String> {
        let expression = &field.expression;
        let value = match &expression.kind {
            ExpressionKind::String(value) => value.clone(),
            ExpressionKind::Name(name) if name.ends_with(".mediaType") => {
                let owner = name.strip_suffix(".mediaType").unwrap_or_default();
                if let Some(constant) = self
                    .capabilities
                    .iter()
                    .find(|capability| capability.name == owner)
                    .and_then(|capability| {
                        capability
                            .constants
                            .iter()
                            .find(|constant| constant.name == "mediaType")
                    })
                {
                    constant.value.to_owned()
                } else {
                    self.codec_media_type(framer, owner, expression.span)?
                }
            }
            _ => {
                self.errors.push(CompileError::new(
                    "framing mediaType must be a string literal or a `name.mediaType` constant",
                    expression.span,
                ));
                return None;
            }
        };
        match MediaType::parse(&value, expression.span) {
            Ok(media) => Some(media.normalized()),
            Err(error) => {
                self.errors
                    .push(CompileError::new(error.message, expression.span));
                None
            }
        }
    }

    fn codec_media_type(&mut self, framer: &FramerDecl, codec: &str, span: Span) -> Option<String> {
        let ids = self
            .program
            .codecs
            .iter()
            .enumerate()
            .map(|(id, codec)| (qualified_name(&codec.namespace, &codec.name.value), id))
            .collect();
        match resolve_visible_name(&ids, codec, &framer.namespace, &framer.namespace_uses) {
            NameResolution::Found(id) => Some(self.program.codecs[id].media_type.value.clone()),
            NameResolution::Missing => {
                self.errors.push(CompileError::new(
                    format!("`{codec}` is neither a codec nor a capability with a mediaType"),
                    span,
                ));
                None
            }
            NameResolution::Ambiguous(names) => {
                self.errors.push(CompileError::new(
                    format!(
                        "codec `{codec}` is ambiguous; use one of {}",
                        names.join(", ")
                    ),
                    span,
                ));
                None
            }
        }
    }

    /// Lower `name.frames(input)` or `name.encode(frame)` for a visible
    /// framing. `None` means the call is not a framing call.
    #[allow(clippy::too_many_arguments, clippy::option_option)]
    pub(super) fn compile_framing_call(
        &mut self,
        current_flow: Option<usize>,
        callee: &Spanned<String>,
        arguments: &[Expression],
        named_arguments: &[ObjectField],
        options: &[ObjectField],
        span: Span,
        locals: &HashMap<String, usize>,
        context: Option<usize>,
    ) -> Option<Option<PlanExpression>> {
        let (name, method) = callee.value.rsplit_once('.')?;
        if !matches!(method, "frames" | "encode") || Self::shadowed_by_local(name, locals) {
            return None;
        }
        let owner = &self.program.flows[current_flow?];
        let ids = self
            .program
            .framers
            .iter()
            .enumerate()
            .map(|(id, framer)| (qualified_name(&framer.namespace, &framer.name.value), id))
            .collect();
        let framer = match resolve_visible_name(&ids, name, &owner.namespace, &owner.namespace_uses)
        {
            NameResolution::Found(id) => id,
            NameResolution::Missing => return None,
            NameResolution::Ambiguous(names) => {
                self.errors.push(CompileError::new(
                    format!(
                        "framing `{name}` is ambiguous; use one of {}",
                        names.join(", ")
                    ),
                    callee.span,
                ));
                return Some(None);
            }
        };
        if arguments.len() != 1 || !named_arguments.is_empty() || !options.is_empty() {
            self.errors.push(CompileError::new(
                format!("framing `{method}` takes exactly one positional argument"),
                span,
            ));
            return Some(None);
        }
        let encode = method == "encode";
        let argument = self.compile_expression(current_flow, &arguments[0], locals, context);
        Some(argument.map(|argument| PlanExpression {
            kind: PlanExpressionKind::FramingCall {
                framer,
                encode,
                argument: Box::new(argument),
            },
            value_type: if encode {
                ValueType::Bytes
            } else {
                ValueType::Stream
            },
            span,
        }))
    }

    /// A local binding shadows a declaration of the same root name.
    pub(super) fn shadowed_by_local(name: &str, locals: &HashMap<String, usize>) -> bool {
        locals.contains_key(name.split('.').next().unwrap_or(name))
    }
}
