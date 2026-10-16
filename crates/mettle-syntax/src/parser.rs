//! Source parsing and grammar rules.

use super::{
    BinaryOperator, CodecDecl, CodecUse, ContextDecl, ContextMember, DeclarationKind, Expression,
    ExpressionKind, FileContextUse, FramerDecl, IfBranch, MettleDecl, ObjectField, ParallelBranch,
    Program, Span, Spanned, Statement, SyntaxError, Token, TokenKind, lex,
};

/// Parse a complete Mettle source file.
///
/// # Errors
///
/// Returns the first lexical or grammatical error with a byte span into `source`.
pub fn parse(source: &str) -> Result<Program, SyntaxError> {
    Parser::new(lex(source)?, source).parse_program()
}

/// Parse one standalone value expression, primarily for CLI flow arguments.
///
/// # Errors
///
/// Returns a lexical or grammatical error when the complete input is not one expression.
pub fn parse_value(source: &str) -> Result<Expression, SyntaxError> {
    let mut parser = Parser::new(lex(source)?, source);
    let expression = parser.parse_expression()?;
    if !parser.at(&TokenKind::End) {
        return Err(parser.expected("the end of the value"));
    }
    Ok(expression)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    cursor: usize,
    source: &'a str,
    suppress_trailing_options: bool,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, source: &'a str) -> Self {
        Self {
            tokens,
            cursor: 0,
            source,
            suppress_trailing_options: false,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_program(mut self) -> Result<Program, SyntaxError> {
        let mut namespace = None;
        let mut namespace_uses = Vec::new();
        let mut contexts = Vec::new();
        let mut file_contexts = Vec::new();
        let mut codecs = Vec::new();
        let mut codec_uses = Vec::new();
        let mut framers = Vec::new();
        let mut flows = Vec::new();
        while !self.at(&TokenKind::End) {
            if self.at(&TokenKind::Namespace) {
                let start = self.advance().span;
                let name = self.take_identifier("a namespace name")?;
                if namespace.replace(name.clone()).is_some() {
                    return Err(SyntaxError::new(
                        "a file may declare only one namespace",
                        start.join(name.span),
                    ));
                }
            } else if self.at(&TokenKind::Use) {
                let start = self.advance().span;
                if self.take_if(&TokenKind::Namespace) {
                    namespace_uses.push(self.take_identifier("a namespace name")?);
                } else if self.take_if(&TokenKind::Codec) {
                    let name = self.take_identifier("a codec name")?;
                    if self.at(&TokenKind::LeftBrace) {
                        let (codec, methods) = self.parse_codec_body(start, name.clone())?;
                        Self::append_codec(
                            codec,
                            methods,
                            namespace.as_ref(),
                            &namespace_uses,
                            &mut flows,
                            &mut codecs,
                        );
                    }
                    codec_uses.push(CodecUse {
                        namespace: namespace
                            .as_ref()
                            .map_or_else(String::new, |name| name.value.clone()),
                        namespace_uses: namespace_uses.clone(),
                        name,
                    });
                } else {
                    self.parse_file_context_use(
                        start,
                        namespace.as_ref(),
                        &namespace_uses,
                        &mut file_contexts,
                        &mut contexts,
                    )?;
                }
            } else if self.at(&TokenKind::Context) {
                let mut context = self.parse_context()?;
                context.namespace = namespace
                    .as_ref()
                    .map_or_else(String::new, |name| name.value.clone());
                context.namespace_uses.clone_from(&namespace_uses);
                contexts.push(context);
            } else if self.at(&TokenKind::Codec) {
                let start = self.advance().span;
                let name = self.take_identifier("a codec name")?;
                let (codec, methods) = self.parse_codec_body(start, name)?;
                Self::append_codec(
                    codec,
                    methods,
                    namespace.as_ref(),
                    &namespace_uses,
                    &mut flows,
                    &mut codecs,
                );
            } else if self.at_framing_declaration() {
                let (mut framer, mut methods) = self.parse_framing()?;
                framer.namespace = namespace
                    .as_ref()
                    .map_or_else(String::new, |name| name.value.clone());
                framer.namespace_uses.clone_from(&namespace_uses);
                if !methods.is_empty() {
                    framer.read_flow = Some(flows.len());
                    framer.write_flow = Some(flows.len() + 1);
                    for mut method in methods.drain(..) {
                        method.namespace.clone_from(&framer.namespace);
                        method.namespace_uses.clone_from(&namespace_uses);
                        flows.push(method);
                    }
                }
                framers.push(framer);
            } else if self.at(&TokenKind::Mettle) {
                let mut flow = self.parse_flow()?;
                flow.namespace = namespace
                    .as_ref()
                    .map_or_else(String::new, |name| name.value.clone());
                flow.namespace_uses.clone_from(&namespace_uses);
                flows.push(flow);
            } else if self.at(&TokenKind::Test) {
                let mut test = self.parse_test()?;
                test.namespace = namespace
                    .as_ref()
                    .map_or_else(String::new, |name| name.value.clone());
                test.namespace_uses.clone_from(&namespace_uses);
                flows.push(test);
            } else {
                let mut flow = self.parse_anonymous_expression_flow()?;
                flow.namespace = namespace
                    .as_ref()
                    .map_or_else(String::new, |name| name.value.clone());
                flow.namespace_uses.clone_from(&namespace_uses);
                flows.push(flow);
            }
        }
        Ok(Program {
            namespace,
            namespace_uses,
            contexts,
            file_contexts,
            codecs,
            codec_uses,
            framers,
            flows,
        })
    }

    fn parse_file_context_use(
        &mut self,
        start: Span,
        namespace: Option<&Spanned<String>>,
        namespace_uses: &[Spanned<String>],
        file_contexts: &mut Vec<FileContextUse>,
        contexts: &mut Vec<ContextDecl>,
    ) -> Result<(), SyntaxError> {
        self.take(&TokenKind::Context)?;
        let name = if self.at(&TokenKind::LeftBrace) {
            None
        } else {
            Some(self.take_identifier("a context name")?)
        };
        if self.at(&TokenKind::LeftBrace) {
            let mut context = self.parse_context_body(start, name.clone())?;
            file_contexts
                .push(name.map_or(FileContextUse::Inline(context.span), FileContextUse::Named));
            context.namespace = namespace.map_or_else(String::new, |name| name.value.clone());
            context.namespace_uses = namespace_uses.to_vec();
            contexts.push(context);
        } else if let Some(name) = name {
            file_contexts.push(FileContextUse::Named(name));
        }
        Ok(())
    }

    fn append_codec(
        mut codec: CodecDecl,
        methods: Vec<MettleDecl>,
        namespace: Option<&Spanned<String>>,
        namespace_uses: &[Spanned<String>],
        flows: &mut Vec<MettleDecl>,
        codecs: &mut Vec<CodecDecl>,
    ) {
        codec.namespace = namespace.map_or_else(String::new, |name| name.value.clone());
        codec.encode_flow = flows.len();
        codec.decode_flow = flows.len() + 1;
        for mut method in methods {
            method.namespace.clone_from(&codec.namespace);
            method.namespace_uses = namespace_uses.to_vec();
            flows.push(method);
        }
        codecs.push(codec);
    }

    fn parse_codec_body(
        &mut self,
        start: Span,
        name: Spanned<String>,
    ) -> Result<(CodecDecl, Vec<MettleDecl>), SyntaxError> {
        self.take(&TokenKind::LeftBrace)?;
        let mut media_type = None;
        let mut aliases = None;
        let mut encode = None;
        let mut decode = None;
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("a codec member or `}`"));
            }
            let member = self.take_identifier("a codec member")?;
            match member.value.as_str() {
                "mediaType" => {
                    let value = self.parse_codec_media_type()?;
                    if media_type.replace(value).is_some() {
                        return Err(SyntaxError::new(
                            "codec mediaType is declared more than once",
                            member.span,
                        ));
                    }
                }
                "aliases" => {
                    let values = self.parse_codec_aliases()?;
                    if aliases.replace(values).is_some() {
                        return Err(SyntaxError::new(
                            "codec aliases are declared more than once",
                            member.span,
                        ));
                    }
                }
                "encode" | "decode" => {
                    let flow = self.parse_codec_method(&name, &member)?;
                    let target = if member.value == "encode" {
                        &mut encode
                    } else {
                        &mut decode
                    };
                    if target.replace(flow).is_some() {
                        return Err(SyntaxError::new(
                            "codec method is declared more than once",
                            member.span,
                        ));
                    }
                }
                _ => {
                    return Err(SyntaxError::new(
                        "expected mediaType, aliases, encode, or decode",
                        member.span,
                    ));
                }
            }
            self.require_separator("codec members")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        let media_type = media_type
            .ok_or_else(|| SyntaxError::new("codec requires mediaType", start.join(end)))?;
        let encode = encode
            .ok_or_else(|| SyntaxError::new("codec requires encode(value)", start.join(end)))?;
        let decode = decode
            .ok_or_else(|| SyntaxError::new("codec requires decode(bytes)", start.join(end)))?;
        let methods = vec![encode, decode];
        Ok((
            CodecDecl {
                namespace: String::new(),
                name,
                media_type,
                aliases: aliases.unwrap_or_default(),
                encode_flow: 0,
                decode_flow: 0,
                span: start.join(end),
            },
            methods,
        ))
    }

    /// `framing` is contextual; it declares only when followed by a name.
    fn at_framing_declaration(&self) -> bool {
        matches!(&self.current().kind, TokenKind::Identifier(name) if name == "framing")
            && matches!(
                self.tokens.get(self.cursor + 1).map(|token| &token.kind),
                Some(TokenKind::Identifier(_))
            )
    }

    fn parse_framing(&mut self) -> Result<(FramerDecl, Vec<MettleDecl>), SyntaxError> {
        let start = self.advance().span;
        let name = self.take_identifier("a framing name")?;
        let mut framer = FramerDecl {
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name: name.clone(),
            builtin: None,
            max_frame_bytes: None,
            read_flow: None,
            write_flow: None,
            span: start,
        };
        if self.take_if(&TokenKind::Equal) {
            let previous = self.suppress_trailing_options;
            self.suppress_trailing_options = true;
            let expression = self.parse_expression();
            self.suppress_trailing_options = previous;
            let expression = expression?;
            if !matches!(expression.kind, ExpressionKind::Call { .. }) {
                return Err(SyntaxError::new(
                    "expected a built-in framing such as delimited(\"\\n\"), lengthPrefixed(u32be), or fixed(16)",
                    expression.span,
                ));
            }
            framer.span = start.join(expression.span);
            framer.builtin = Some(expression);
            return Ok((framer, Vec::new()));
        }
        if !self.at(&TokenKind::LeftBrace) {
            return Err(self.expected("`=` and a built-in framing, or a `{` custom framing body"));
        }
        self.advance();
        let mut read = None;
        let mut write = None;
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("a framing member or `}`"));
            }
            let member = self.take_identifier("a framing member")?;
            match member.value.as_str() {
                "maxFrameBytes" => {
                    if framer.max_frame_bytes.is_some() {
                        return Err(SyntaxError::new(
                            "maxFrameBytes is declared more than once",
                            member.span,
                        ));
                    }
                    self.take(&TokenKind::Colon)?;
                    framer.max_frame_bytes = Some(self.parse_expression()?);
                }
                "read" | "write" => {
                    let mut flow = self.parse_codec_method(&name, &member)?;
                    flow.kind = DeclarationKind::Framing;
                    let slot = if member.value == "read" {
                        &mut read
                    } else {
                        &mut write
                    };
                    if slot.replace(flow).is_some() {
                        return Err(SyntaxError::new(
                            format!(
                                "framing method `{}` is declared more than once",
                                member.value
                            ),
                            member.span,
                        ));
                    }
                }
                "delimiter" | "mediaType" => {
                    return Err(SyntaxError::new(
                        format!(
                            "`{}` is not a custom framing member: use `framing {} = delimited(...)` for delimited frames, and return mediaType per frame from read",
                            member.value, name.value
                        ),
                        member.span,
                    ));
                }
                _ => {
                    return Err(SyntaxError::new(
                        "expected maxFrameBytes, read, or write",
                        member.span,
                    ));
                }
            }
            self.require_separator("framing members")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        framer.span = start.join(end);
        let read = read
            .ok_or_else(|| SyntaxError::new("custom framing requires read(input)", framer.span))?;
        let write = write
            .ok_or_else(|| SyntaxError::new("custom framing requires write(frame)", framer.span))?;
        Ok((framer, vec![read, write]))
    }

    fn parse_codec_media_type(&mut self) -> Result<Spanned<String>, SyntaxError> {
        self.take(&TokenKind::Colon)?;
        let value = self.advance();
        let TokenKind::String(value_string) = value.kind else {
            return Err(SyntaxError::new(
                "codec mediaType must be a string literal",
                value.span,
            ));
        };
        Ok(Spanned {
            value: value_string,
            span: value.span,
        })
    }

    fn parse_codec_aliases(&mut self) -> Result<Vec<Spanned<String>>, SyntaxError> {
        self.take(&TokenKind::Colon)?;
        self.take(&TokenKind::LeftBracket)?;
        let mut values = Vec::new();
        while !self.at(&TokenKind::RightBracket) {
            let value = self.advance();
            let TokenKind::String(value_string) = value.kind else {
                return Err(SyntaxError::new(
                    "codec aliases must be string literals",
                    value.span,
                ));
            };
            values.push(Spanned {
                value: value_string,
                span: value.span,
            });
            if !self.take_if(&TokenKind::Comma) {
                break;
            }
        }
        self.take(&TokenKind::RightBracket)?;
        Ok(values)
    }

    fn parse_codec_method(
        &mut self,
        name: &Spanned<String>,
        member: &Spanned<String>,
    ) -> Result<MettleDecl, SyntaxError> {
        self.take(&TokenKind::LeftParen)?;
        let parameter = self.take_identifier("a codec input parameter")?;
        self.take(&TokenKind::RightParen)?;
        let (body, end) = self.parse_statement_block()?;
        Ok(MettleDecl {
            kind: DeclarationKind::Codec,
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name: Some(Spanned {
                value: format!("{}.{}", name.value, member.value),
                span: member.span,
            }),
            parameters: vec![parameter],
            body,
            span: member.span.join(end),
        })
    }

    fn parse_context(&mut self) -> Result<ContextDecl, SyntaxError> {
        let start = self.take(&TokenKind::Context)?.span;
        let name = self.take_identifier("a context name")?;
        self.parse_context_body(start, Some(name))
    }

    fn parse_context_body(
        &mut self,
        start: Span,
        name: Option<Spanned<String>>,
    ) -> Result<ContextDecl, SyntaxError> {
        self.take(&TokenKind::LeftBrace)?;
        let mut members = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("a context member or `}`"));
            }
            if self.at(&TokenKind::Use) {
                let start = self.advance().span;
                self.take(&TokenKind::Context)?;
                let name = self.take_identifier("a context name")?;
                members.push(ContextMember::UseContext {
                    span: start.join(name.span),
                    name,
                });
            } else if self.at(&TokenKind::Defaults) {
                let defaults = self.advance().span;
                let capability = self.take_identifier("a capability name")?;
                let (fields, block_span) = self.parse_object_fields()?;
                members.push(ContextMember::Defaults {
                    capability,
                    fields,
                    span: defaults.join(block_span),
                });
            } else {
                members.push(ContextMember::Field(self.parse_object_field()?));
            }
            self.require_separator("context members")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        Ok(ContextDecl {
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name,
            members,
            span: start.join(end),
        })
    }

    fn parse_flow(&mut self) -> Result<MettleDecl, SyntaxError> {
        let start = self.take(&TokenKind::Mettle)?.span;
        if self.at(&TokenKind::LeftBrace) {
            let (body, end) = self.parse_statement_block()?;
            return Ok(MettleDecl {
                kind: DeclarationKind::Flow,
                namespace: String::new(),
                namespace_uses: Vec::new(),
                name: None,
                parameters: Vec::new(),
                body,
                span: start.join(end),
            });
        }

        let name = self.take_identifier("a flow name or `{`")?;
        let mut parameters = Vec::new();
        if self.take_if(&TokenKind::LeftParen) {
            if !self.at(&TokenKind::RightParen) {
                loop {
                    parameters.push(self.take_identifier("a parameter name")?);
                    if !self.take_if(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.take(&TokenKind::RightParen)?;
        }
        if self.take_if(&TokenKind::Equal) {
            let expression = self.parse_expression()?;
            let span = start.join(expression.span);
            return Ok(MettleDecl {
                kind: DeclarationKind::Flow,
                namespace: String::new(),
                namespace_uses: Vec::new(),
                name: Some(name),
                parameters,
                body: vec![Statement::Return { expression, span }],
                span,
            });
        }

        let (body, end) = self.parse_statement_block()?;
        Ok(MettleDecl {
            kind: DeclarationKind::Flow,
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name: Some(name),
            parameters,
            body,
            span: start.join(end),
        })
    }

    fn parse_test(&mut self) -> Result<MettleDecl, SyntaxError> {
        let start = self.take(&TokenKind::Test)?.span;
        let parenthesized = self.take_if(&TokenKind::LeftParen);
        let token = self.advance();
        let TokenKind::String(value) = token.kind else {
            return Err(SyntaxError::new("expected a test name string", token.span));
        };
        if value.trim().is_empty() {
            return Err(SyntaxError::new("test name cannot be empty", token.span));
        }
        if parenthesized {
            self.take(&TokenKind::RightParen)?;
        }
        let (body, end) = self.parse_statement_block()?;
        Ok(MettleDecl {
            kind: DeclarationKind::Test,
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name: Some(Spanned {
                value,
                span: token.span,
            }),
            parameters: Vec::new(),
            body,
            span: start.join(end),
        })
    }

    fn parse_statement_block(&mut self) -> Result<(Vec<Statement>, Span), SyntaxError> {
        self.take(&TokenKind::LeftBrace)?;
        let mut body = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("a statement or `}`"));
            }
            body.push(self.parse_statement()?);
            self.require_newline("statements")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        Ok((body, end))
    }

    fn parse_anonymous_expression_flow(&mut self) -> Result<MettleDecl, SyntaxError> {
        let expression = self.parse_expression()?;
        if !matches!(expression.kind, ExpressionKind::Call { .. }) {
            return Err(SyntaxError::new(
                "a top-level anonymous flow must be a call expression",
                expression.span,
            ));
        }
        let span = expression.span;
        Ok(MettleDecl {
            kind: DeclarationKind::Flow,
            namespace: String::new(),
            namespace_uses: Vec::new(),
            name: None,
            parameters: Vec::new(),
            body: vec![Statement::Return { expression, span }],
            span,
        })
    }

    fn parse_statement(&mut self) -> Result<Statement, SyntaxError> {
        if self.at(&TokenKind::If) {
            let start = self.advance().span;
            let mut branch_start = start;
            let mut branches = Vec::new();
            let mut else_body = None;
            loop {
                self.take(&TokenKind::LeftParen)?;
                let condition = self.parse_expression()?;
                self.take(&TokenKind::RightParen)?;
                let (body, end) = self.parse_statement_block()?;
                branches.push(IfBranch {
                    condition,
                    body,
                    span: branch_start.join(end),
                });
                if !self.take_if(&TokenKind::Else) {
                    break;
                }
                if self.at(&TokenKind::If) {
                    branch_start = self.advance().span;
                    continue;
                }
                else_body = Some(self.parse_statement_block()?.0);
                break;
            }
            let end = self.tokens[self.cursor - 1].span;
            return Ok(Statement::If {
                branches,
                else_body,
                span: start.join(end),
            });
        }
        if self.at(&TokenKind::Use) {
            let start = self.advance().span;
            self.take(&TokenKind::Context)?;
            let name = self.take_identifier("a context name")?;
            return Ok(Statement::UseContext {
                span: start.join(name.span),
                name,
            });
        }

        if self.at(&TokenKind::Break) {
            return Ok(Statement::Break(self.advance().span));
        }
        if self.at(&TokenKind::Yield) {
            let start = self.advance().span;
            let expression = self.parse_expression()?;
            return Ok(Statement::Yield {
                span: start.join(expression.span),
                expression,
            });
        }
        if self.at(&TokenKind::Return) {
            let start = self.advance().span;
            let expression = self.parse_expression()?;
            return Ok(Statement::Return {
                span: start.join(expression.span),
                expression,
            });
        }

        if self.at(&TokenKind::Assert) {
            let start = self.advance().span;
            self.take(&TokenKind::LeftParen)?;
            let expression = self.parse_expression()?;
            let message = if self.take_if(&TokenKind::Comma) {
                let message = self.parse_expression()?;
                if !matches!(message.kind, ExpressionKind::String(_)) {
                    return Err(SyntaxError::new(
                        "assertion message must be a string literal",
                        message.span,
                    ));
                }
                Some(message)
            } else {
                None
            };
            let end = self.take(&TokenKind::RightParen)?.span;
            return Ok(Statement::Assert {
                expression,
                message,
                span: start.join(end),
            });
        }

        if matches!(
            self.current().kind,
            TokenKind::Identifier(_) | TokenKind::Source
        ) && self.peek_at(1, &TokenKind::Equal)
        {
            let name = self.take_identifier("a binding name")?;
            self.take(&TokenKind::Equal)?;
            let expression = self.parse_expression()?;
            return Ok(Statement::Bind {
                span: name.span.join(expression.span),
                name,
                expression,
            });
        }

        Ok(Statement::Expression(self.parse_expression()?))
    }

    fn parse_expression(&mut self) -> Result<Expression, SyntaxError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_and()?;
        while self.take_if(&TokenKind::Or) {
            let right = self.parse_and()?;
            left = binary(left, BinaryOperator::Or, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_comparison()?;
        while self.take_if(&TokenKind::And) {
            let right = self.parse_comparison()?;
            left = binary(left, BinaryOperator::And, right);
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expression, SyntaxError> {
        let left = self.parse_unary()?;
        let operator = match self.current().kind {
            TokenKind::EqualEqual => BinaryOperator::Equal,
            TokenKind::BangEqual => BinaryOperator::NotEqual,
            TokenKind::Less => BinaryOperator::Less,
            TokenKind::LessEqual => BinaryOperator::LessEqual,
            TokenKind::Greater => BinaryOperator::Greater,
            TokenKind::GreaterEqual => BinaryOperator::GreaterEqual,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_unary()?;
        Ok(binary(left, operator, right))
    }

    fn parse_unary(&mut self) -> Result<Expression, SyntaxError> {
        if self.at(&TokenKind::Not) {
            let start = self.advance().span;
            let value = self.parse_unary()?;
            return Ok(Expression {
                span: start.join(value.span),
                kind: ExpressionKind::Not(Box::new(value)),
            });
        }
        if self.at(&TokenKind::Minus) {
            let start = self.advance().span;
            let value = self.parse_unary()?;
            return Ok(Expression {
                span: start.join(value.span),
                kind: ExpressionKind::Negate(Box::new(value)),
            });
        }
        self.parse_type_operation()
    }

    fn parse_type_operation(&mut self) -> Result<Expression, SyntaxError> {
        let mut expression = self.parse_primary_expression()?;
        while self.at(&TokenKind::Is) || self.at(&TokenKind::As) {
            let cast = self.advance().kind == TokenKind::As;
            let token = self.advance();
            let name = match &token.kind {
                TokenKind::Identifier(name) => name.as_str(),
                TokenKind::Null => "null",
                TokenKind::Source => "source",
                _ => {
                    return Err(SyntaxError::new(
                        "expected a built-in value kind after `is` or `as`",
                        token.span,
                    ));
                }
            };
            let kind = super::ValueKind::parse(name).ok_or_else(|| {
                SyntaxError::new("unknown value kind; use null, boolean, integer, number, string, bytes, duration, array, object, or source", token.span)
            })?;
            let span = expression.span.join(token.span);
            expression = Expression {
                kind: ExpressionKind::TypeOperation {
                    value: Box::new(expression),
                    target: Spanned {
                        value: kind,
                        span: token.span,
                    },
                    cast,
                },
                span,
            };
            expression = self.parse_member_access(expression)?;
        }
        Ok(expression)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_primary_expression(&mut self) -> Result<Expression, SyntaxError> {
        let mut token = self.advance();
        // `source` is contextual: existing variables, parameters and fields may
        // use the name; only `source { ... }` creates a producer.
        if token.kind == TokenKind::Source && !self.at(&TokenKind::LeftBrace) {
            token.kind = TokenKind::Identifier("source".to_owned());
        }
        let mut expression = match token.kind {
            TokenKind::Source if self.at(&TokenKind::LeftBrace) => {
                let (body, end) = self.parse_statement_block()?;
                literal(ExpressionKind::Source(body), token.span.join(end))
            }
            TokenKind::Within => self.parse_within(token.span)?,
            TokenKind::For => self.parse_for(token.span)?,
            TokenKind::Retry => self.parse_retry(token.span)?,
            TokenKind::Parallel => self.parse_parallel(token.span)?,
            TokenKind::Rate => self.parse_rate(token.span)?,
            TokenKind::Concurrency => self.parse_concurrency(token.span)?,
            TokenKind::Fail => self.parse_fail(token.span)?,
            TokenKind::Null => literal(ExpressionKind::Null, token.span),
            TokenKind::True => literal(ExpressionKind::Boolean(true), token.span),
            TokenKind::False => literal(ExpressionKind::Boolean(false), token.span),
            TokenKind::Integer(value) => literal(ExpressionKind::Integer(value), token.span),
            TokenKind::Float(value) => literal(ExpressionKind::Float(value), token.span),
            TokenKind::DurationNanos(value) => {
                literal(ExpressionKind::DurationNanos(value), token.span)
            }
            TokenKind::String(value) => literal(ExpressionKind::String(value), token.span),
            TokenKind::LeftParen => {
                let mut value = self.parse_expression()?;
                value.span = token.span.join(self.take(&TokenKind::RightParen)?.span);
                value
            }
            TokenKind::Identifier(value) => {
                let mut name = value;
                let mut span = token.span;
                while self.at(&TokenKind::Dot)
                    && matches!(
                        self.tokens.get(self.cursor + 1).map(|token| &token.kind),
                        Some(TokenKind::Identifier(_) | TokenKind::Source)
                    )
                {
                    self.advance();
                    let member = self.take_identifier("a name after `.`")?;
                    name.push('.');
                    name.push_str(&member.value);
                    span = span.join(member.span);
                }
                literal(ExpressionKind::Name(name), span)
            }
            TokenKind::LeftBracket => self.parse_array(token.span)?,
            TokenKind::LeftBrace => {
                let (fields, end) = self.parse_object_fields_after_open()?;
                literal(ExpressionKind::Object(fields), token.span.join(end))
            }
            _ => return Err(SyntaxError::new("expected an expression", token.span)),
        };

        if self.at(&TokenKind::LeftParen) {
            let ExpressionKind::Name(name) = expression.kind else {
                return Err(SyntaxError::new(
                    "only a named flow or capability operation can be called",
                    expression.span,
                ));
            };
            let callee = Spanned {
                value: name,
                span: expression.span,
            };
            self.advance();
            let mut arguments = Vec::new();
            let mut named_arguments = Vec::new();
            if !self.at(&TokenKind::RightParen) {
                loop {
                    if matches!(
                        self.current().kind,
                        TokenKind::Identifier(_) | TokenKind::Source
                    ) && self.peek_at(1, &TokenKind::Colon)
                    {
                        named_arguments.push(self.parse_object_field()?);
                    } else if named_arguments.is_empty() {
                        arguments.push(self.parse_expression()?);
                    } else {
                        return Err(SyntaxError::new(
                            "positional arguments must come before named arguments",
                            self.current().span,
                        ));
                    }
                    if !self.take_if(&TokenKind::Comma) {
                        break;
                    }
                    if self.at(&TokenKind::RightParen) {
                        break;
                    }
                }
            }
            let close = self.take(&TokenKind::RightParen)?.span;
            let (options, end) =
                if self.at(&TokenKind::LeftBrace) && !self.suppress_trailing_options {
                    self.parse_object_fields()?
                } else {
                    (Vec::new(), close)
                };
            expression = Expression {
                span: callee.span.join(end),
                kind: ExpressionKind::Call {
                    callee,
                    arguments,
                    named_arguments,
                    options,
                },
            };
        }

        self.parse_member_access(expression)
    }

    fn parse_member_access(
        &mut self,
        mut expression: Expression,
    ) -> Result<Expression, SyntaxError> {
        loop {
            if self.take_if(&TokenKind::Dot) {
                let member = self.take_identifier("a member name after `.`")?;
                let span = expression.span.join(member.span);
                expression = Expression {
                    kind: ExpressionKind::Member {
                        value: Box::new(expression),
                        member,
                    },
                    span,
                };
            } else if self.take_if(&TokenKind::LeftBracket) {
                let index = self.parse_expression()?;
                let close = self.take(&TokenKind::RightBracket)?.span;
                let span = expression.span.join(close);
                expression = Expression {
                    kind: ExpressionKind::Index {
                        value: Box::new(expression),
                        index: Box::new(index),
                    },
                    span,
                };
            } else {
                break;
            }
        }

        Ok(expression)
    }

    fn parse_within(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut options = self.parse_policy_options()?;
        let timeout = Self::required_policy_option(&mut options, "timeout", start)?;
        Self::reject_extra_policy_options(&options)?;
        let (statements, end) = self.parse_statement_block()?;
        let body = Expression {
            kind: ExpressionKind::Block(statements),
            span: start.join(end),
        };
        Ok(Expression {
            kind: ExpressionKind::Within {
                timeout: Box::new(timeout),
                body: Box::new(body),
            },
            span: start.join(end),
        })
    }

    fn parse_fail(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        self.take(&TokenKind::LeftParen)?;
        let message = self.parse_expression()?;
        let end = self.take(&TokenKind::RightParen)?.span;
        Ok(Expression {
            kind: ExpressionKind::Fail(Box::new(message)),
            span: start.join(end),
        })
    }

    fn parse_for(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let first = self.take_identifier("a loop binding")?;
        let (key, value) = if self.take_if(&TokenKind::Comma) {
            (Some(first), self.take_identifier("a loop value binding")?)
        } else {
            (None, first)
        };
        self.take(&TokenKind::In)?;
        let previous = self.suppress_trailing_options;
        self.suppress_trailing_options = true;
        let iterable = self.parse_expression()?;
        self.suppress_trailing_options = previous;
        let (body, end) = self.parse_statement_block()?;
        Ok(Expression {
            kind: ExpressionKind::For {
                key,
                value,
                iterable: Box::new(iterable),
                body,
            },
            span: start.join(end),
        })
    }

    fn parse_retry(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut options = self.parse_policy_options()?;
        let attempts = Self::required_policy_option(&mut options, "attempts", start)?;
        let delay = Self::optional_policy_option(&mut options, "delay").map(Box::new);
        Self::reject_extra_policy_options(&options)?;
        let (statements, end) = self.parse_statement_block()?;
        let body = Expression {
            kind: ExpressionKind::Block(statements),
            span: start.join(end),
        };
        Ok(Expression {
            kind: ExpressionKind::Retry {
                attempts: Box::new(attempts),
                delay,
                body: Box::new(body),
            },
            span: start.join(end),
        })
    }

    fn parse_parallel(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut options = if self.at(&TokenKind::LeftParen) {
            self.parse_policy_options()?
        } else {
            Vec::new()
        };
        let limit = Self::optional_policy_option(&mut options, "limit").map(Box::new);
        Self::reject_extra_policy_options(&options)?;
        self.take(&TokenKind::LeftBrace)?;
        let mut branches = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("a parallel branch or `}`"));
            }
            let name = if matches!(
                self.current().kind,
                TokenKind::Identifier(_) | TokenKind::Source
            ) && self.peek_at(1, &TokenKind::Colon)
            {
                let name = self.take_identifier("a parallel branch name")?;
                self.take(&TokenKind::Colon)?;
                Some(name)
            } else {
                None
            };
            branches.push(ParallelBranch {
                name,
                expression: self.parse_expression()?,
            });
            self.require_separator("parallel branches")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        Ok(Expression {
            kind: ExpressionKind::Parallel { limit, branches },
            span: start.join(end),
        })
    }

    fn parse_rate(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut options = self.parse_policy_options()?;
        let target = Self::required_policy_option(&mut options, "target", start)?;
        let period = Self::required_policy_option(&mut options, "period", start)?;
        let duration = Self::required_policy_option(&mut options, "duration", start)?;
        let limit = Self::optional_policy_option(&mut options, "limit").map(Box::new);
        Self::reject_extra_policy_options(&options)?;
        let (statements, end) = self.parse_statement_block()?;
        let body = Expression {
            kind: ExpressionKind::Block(statements),
            span: start.join(end),
        };
        Ok(Expression {
            kind: ExpressionKind::Rate {
                target: Box::new(target),
                period: Box::new(period),
                duration: Box::new(duration),
                limit,
                body: Box::new(body),
            },
            span: start.join(end),
        })
    }

    fn parse_concurrency(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut options = self.parse_policy_options()?;
        let limit = Self::required_policy_option(&mut options, "limit", start)?;
        let duration = Self::required_policy_option(&mut options, "duration", start)?;
        Self::reject_extra_policy_options(&options)?;
        let (statements, end) = self.parse_statement_block()?;
        let body = Expression {
            kind: ExpressionKind::Block(statements),
            span: start.join(end),
        };
        Ok(Expression {
            kind: ExpressionKind::Concurrency {
                limit: Box::new(limit),
                duration: Box::new(duration),
                body: Box::new(body),
            },
            span: start.join(end),
        })
    }

    fn parse_policy_options(&mut self) -> Result<Vec<ObjectField>, SyntaxError> {
        self.take(&TokenKind::LeftParen)?;
        let mut options = Vec::new();
        while !self.at(&TokenKind::RightParen) {
            let name = self.take_identifier("a policy option name")?;
            self.take(&TokenKind::Colon)?;
            let expression = self.parse_expression()?;
            if options
                .iter()
                .any(|field: &ObjectField| field.name.value == name.value)
            {
                return Err(SyntaxError::new(
                    format!("policy option `{}` was supplied more than once", name.value),
                    name.span,
                ));
            }
            options.push(ObjectField {
                span: name.span.join(expression.span),
                name,
                expression,
            });
            if !self.take_if(&TokenKind::Comma) {
                break;
            }
        }
        self.take(&TokenKind::RightParen)?;
        Ok(options)
    }

    fn optional_policy_option(options: &mut Vec<ObjectField>, name: &str) -> Option<Expression> {
        options
            .iter()
            .position(|field| field.name.value == name)
            .map(|index| options.remove(index).expression)
    }

    fn required_policy_option(
        options: &mut Vec<ObjectField>,
        name: &str,
        span: Span,
    ) -> Result<Expression, SyntaxError> {
        Self::optional_policy_option(options, name)
            .ok_or_else(|| SyntaxError::new(format!("missing `{name}` policy option"), span))
    }

    fn reject_extra_policy_options(options: &[ObjectField]) -> Result<(), SyntaxError> {
        if let Some(field) = options.first() {
            return Err(SyntaxError::new(
                format!("unknown policy option `{}`", field.name.value),
                field.name.span,
            ));
        }
        Ok(())
    }

    fn parse_array(&mut self, start: Span) -> Result<Expression, SyntaxError> {
        let mut values = Vec::new();
        if !self.at(&TokenKind::RightBracket) {
            loop {
                values.push(self.parse_expression()?);
                if !self.take_if(&TokenKind::Comma) {
                    break;
                }
                if self.at(&TokenKind::RightBracket) {
                    break;
                }
            }
        }
        let end = self.take(&TokenKind::RightBracket)?.span;
        Ok(Expression {
            kind: ExpressionKind::Array(values),
            span: start.join(end),
        })
    }

    fn parse_object_fields(&mut self) -> Result<(Vec<ObjectField>, Span), SyntaxError> {
        self.take(&TokenKind::LeftBrace)?;
        self.parse_object_fields_after_open()
    }

    fn parse_object_fields_after_open(&mut self) -> Result<(Vec<ObjectField>, Span), SyntaxError> {
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::End) {
                return Err(self.expected("an object field or `}`"));
            }
            fields.push(self.parse_object_field()?);
            self.require_separator("object fields")?;
        }
        let end = self.take(&TokenKind::RightBrace)?.span;
        Ok((fields, end))
    }

    fn parse_object_field(&mut self) -> Result<ObjectField, SyntaxError> {
        let token = self.advance();
        let name = match token.kind {
            TokenKind::Source => Spanned {
                value: "source".to_owned(),
                span: token.span,
            },
            TokenKind::Identifier(value) | TokenKind::String(value) => Spanned {
                value,
                span: token.span,
            },
            _ => {
                return Err(SyntaxError::new(
                    "expected an object field name",
                    token.span,
                ));
            }
        };
        self.take(&TokenKind::Colon)?;
        let expression = self.parse_expression()?;
        Ok(ObjectField {
            span: name.span.join(expression.span),
            name,
            expression,
        })
    }

    fn take_identifier(&mut self, expected: &'static str) -> Result<Spanned<String>, SyntaxError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Source => Ok(Spanned {
                value: "source".to_owned(),
                span: token.span,
            }),
            TokenKind::Identifier(value) => Ok(Spanned {
                value,
                span: token.span,
            }),
            _ => Err(SyntaxError::new(format!("expected {expected}"), token.span)),
        }
    }

    fn take(&mut self, expected: &TokenKind) -> Result<Token, SyntaxError> {
        if self.at(expected) {
            Ok(self.advance())
        } else {
            Err(self.expected(token_description(expected)))
        }
    }

    fn take_if(&mut self, expected: &TokenKind) -> bool {
        if self.at(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &TokenKind) -> bool {
        same_variant(&self.current().kind, expected)
    }

    fn peek_at(&self, offset: usize, expected: &TokenKind) -> bool {
        self.tokens
            .get(self.cursor + offset)
            .is_some_and(|token| same_variant(&token.kind, expected))
    }

    fn current(&self) -> &Token {
        &self.tokens[self.cursor]
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.cursor].clone();
        if !matches!(token.kind, TokenKind::End) {
            self.cursor += 1;
        }
        token
    }

    fn expected(&self, expected: &'static str) -> SyntaxError {
        SyntaxError::new(format!("expected {expected}"), self.current().span)
    }

    fn has_newline_before_current(&self) -> bool {
        let previous_end = self.tokens[self.cursor - 1].span.end;
        self.source[previous_end..self.current().span.start].contains('\n')
    }

    fn require_newline(&self, what: &'static str) -> Result<(), SyntaxError> {
        if self.at(&TokenKind::RightBrace) || self.has_newline_before_current() {
            Ok(())
        } else {
            Err(SyntaxError::new(
                format!("separate {what} with a newline"),
                self.current().span,
            ))
        }
    }

    fn require_separator(&mut self, what: &'static str) -> Result<(), SyntaxError> {
        if self.take_if(&TokenKind::Comma)
            || self.at(&TokenKind::RightBrace)
            || self.has_newline_before_current()
        {
            Ok(())
        } else {
            Err(SyntaxError::new(
                format!("separate {what} with a comma or newline"),
                self.current().span,
            ))
        }
    }
}

fn binary(left: Expression, operator: BinaryOperator, right: Expression) -> Expression {
    Expression {
        span: left.span.join(right.span),
        kind: ExpressionKind::Binary {
            left: Box::new(left),
            operator,
            right: Box::new(right),
        },
    }
}

fn literal(kind: ExpressionKind, span: Span) -> Expression {
    Expression { kind, span }
}

fn same_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

const fn token_description(token: &TokenKind) -> &'static str {
    match token {
        TokenKind::If => "`if`",
        TokenKind::For => "`for`",
        TokenKind::In => "`in`",
        TokenKind::Else => "`else`",
        TokenKind::And => "`and`",
        TokenKind::Or => "`or`",
        TokenKind::Not => "`not`",
        TokenKind::Is => "`is`",
        TokenKind::As => "`as`",
        TokenKind::Minus => "`-`",
        TokenKind::Mettle => "`flow`",
        TokenKind::Test => "`test`",
        TokenKind::Context => "`context`",
        TokenKind::Codec => "`codec`",
        TokenKind::Framing => "`framing`",
        TokenKind::Namespace => "`namespace`",
        TokenKind::Defaults => "`defaults`",
        TokenKind::Use => "`use`",
        TokenKind::Return => "`return`",
        TokenKind::Break => "`break`",
        TokenKind::Yield => "`yield`",
        TokenKind::Source => "`source`",
        TokenKind::Assert => "`assert`",
        TokenKind::Fail => "`fail`",
        TokenKind::Within => "`within`",
        TokenKind::Retry => "`retry`",
        TokenKind::Parallel => "`parallel`",
        TokenKind::Rate => "`rate`",
        TokenKind::Concurrency => "`concurrency`",
        TokenKind::True => "`true`",
        TokenKind::False => "`false`",
        TokenKind::Null => "`null`",
        TokenKind::Identifier(_) => "an identifier",
        TokenKind::Integer(_) => "an integer",
        TokenKind::Float(_) => "a number",
        TokenKind::DurationNanos(_) => "a duration",
        TokenKind::String(_) => "a string",
        TokenKind::LeftBrace => "`{`",
        TokenKind::RightBrace => "`}`",
        TokenKind::LeftBracket => "`[`",
        TokenKind::RightBracket => "`]`",
        TokenKind::LeftParen => "`(`",
        TokenKind::RightParen => "`)`",
        TokenKind::Comma => "`,`",
        TokenKind::Colon => "`:`",
        TokenKind::Equal => "`=`",
        TokenKind::EqualEqual => "`==`",
        TokenKind::BangEqual => "`!=`",
        TokenKind::Less => "`<`",
        TokenKind::LessEqual => "`<=`",
        TokenKind::Greater => "`>`",
        TokenKind::GreaterEqual => "`>=`",
        TokenKind::Dot => "`.`",
        TokenKind::End => "the end of the file",
    }
}
