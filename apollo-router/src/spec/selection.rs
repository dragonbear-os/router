use crate::json_ext::Object;
use crate::{FieldType, Schema, SpecError};
use apollo_parser::ast::{self, Value};
use serde_json_bytes::ByteString;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Selection {
    Field {
        name: ByteString,
        alias: Option<ByteString>,
        selection_set: Option<Vec<Selection>>,
        field_type: FieldType,
        skip: Skip,
        include: Include,
    },
    InlineFragment {
        // Optional in specs but we fill it with the current type if not specified
        type_condition: String,
        skip: Skip,
        include: Include,
        known_type: bool,
        selection_set: Vec<Selection>,
    },
    FragmentSpread {
        name: String,
        known_type: Option<String>,
        skip: Skip,
        include: Include,
    },
}

impl Selection {
    pub(crate) fn from_ast(
        selection: ast::Selection,
        current_type: &FieldType,
        schema: &Schema,
        mut count: usize,
    ) -> Result<Option<Self>, SpecError> {
        // The RECURSION_LIMIT is chosen to be:
        //   < # expected to cause stack overflow &&
        //   > # expected in a legitimate query
        const RECURSION_LIMIT: usize = 512;
        if count > RECURSION_LIMIT {
            tracing::error!("selection processing recursion limit({RECURSION_LIMIT}) exceeded");
            return Err(SpecError::RecursionLimitExceeded);
        }
        count += 1;
        let selection = match selection {
            // Spec: https://spec.graphql.org/draft/#Field
            ast::Selection::Field(field) => {
                let skip = field
                    .directives()
                    .map(|directives| {
                        // skip directives have been validated before, so we're safe here
                        for directive in directives.directives() {
                            if let Some(skip) = parse_skip(&directive) {
                                return skip;
                            }
                        }
                        Skip::No
                    })
                    .unwrap_or(Skip::No);
                if skip.statically_skipped() {
                    return Ok(None);
                }

                let include = field
                    .directives()
                    .map(|directives| {
                        for directive in directives.directives() {
                            // include directives have been validated before, so we're safe here
                            if let Some(include) = parse_include(&directive) {
                                return include;
                            }
                        }
                        Include::Yes
                    })
                    .unwrap_or(Include::Yes);
                if include.statically_skipped() {
                    return Ok(None);
                }

                let field_name = field
                    .name()
                    .expect("the node Name is not optional in the spec; qed")
                    .text()
                    .to_string();

                let field_type = if field_name.as_str() == "__typename" {
                    FieldType::String
                } else if field_name == "__schema" {
                    FieldType::Introspection("__Schema".to_string())
                } else if field_name == "__type" {
                    FieldType::Introspection("__Type".to_string())
                } else {
                    current_type
                        .inner_type_name()
                        .and_then(|name| {
                            //looking into object types
                            schema
                                .object_types
                                .get(name)
                                .and_then(|ty| ty.field(&field_name))
                                // otherwise, it might be an interface
                                .or_else(|| {
                                    schema
                                        .interfaces
                                        .get(name)
                                        .and_then(|ty| ty.field(&field_name))
                                })
                        })
                        .ok_or_else(|| SpecError::InvalidType(current_type.to_string()))?
                        .clone()
                };

                let alias = field.alias().map(|x| x.name().unwrap().text().to_string());

                let selection_set = if field_type.is_builtin_scalar() {
                    None
                } else {
                    match field.selection_set() {
                        None => None,
                        Some(selection_set) => selection_set
                            .selections()
                            .map(|selection| {
                                Selection::from_ast(selection, &field_type, schema, count)
                            })
                            .collect::<Result<Vec<Option<_>>, _>>()?
                            .into_iter()
                            .flatten()
                            .collect::<Vec<Selection>>()
                            .into(),
                    }
                };

                Some(Self::Field {
                    alias: alias.map(|alias| alias.into()),
                    name: field_name.into(),
                    selection_set,
                    field_type,
                    skip,
                    include,
                })
            }
            // Spec: https://spec.graphql.org/draft/#InlineFragment
            ast::Selection::InlineFragment(inline_fragment) => {
                let skip = inline_fragment
                    .directives()
                    .map(|directives| {
                        // skip directives have been validated before, so we're safe here
                        for directive in directives.directives() {
                            if let Some(skip) = parse_skip(&directive) {
                                return skip;
                            }
                        }
                        Skip::No
                    })
                    .unwrap_or(Skip::No);
                if skip.statically_skipped() {
                    return Ok(None);
                }

                let include = inline_fragment
                    .directives()
                    .map(|directives| {
                        for directive in directives.directives() {
                            // include directives have been validated before, so we're safe here
                            if let Some(include) = parse_include(&directive) {
                                return include;
                            }
                        }
                        Include::Yes
                    })
                    .unwrap_or(Include::Yes);
                if include.statically_skipped() {
                    return Ok(None);
                }

                let type_condition = inline_fragment
                    .type_condition()
                    .map(|condition| {
                        condition
                            .named_type()
                            .expect("TypeCondition must specify the NamedType it applies to; qed")
                            .name()
                            .expect("the node Name is not optional in the spec; qed")
                            .text()
                            .to_string()
                    })
                    // if we can't get a type name from the current type, that means we're applying
                    // a fragment onto a scalar
                    .or_else(|| current_type.inner_type_name().map(|s| s.to_string()))
                    .ok_or_else(|| SpecError::InvalidType(current_type.to_string()))?;

                let fragment_type = FieldType::Named(type_condition.clone());

                let selection_set = inline_fragment
                    .selection_set()
                    .expect("the node SelectionSet is not optional in the spec; qed")
                    .selections()
                    .map(|selection| Selection::from_ast(selection, &fragment_type, schema, count))
                    .collect::<Result<Vec<Option<_>>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();

                let known_type = current_type.inner_type_name() == Some(type_condition.as_str());
                Some(Self::InlineFragment {
                    type_condition,
                    selection_set,
                    skip,
                    include,
                    known_type,
                })
            }
            // Spec: https://spec.graphql.org/draft/#FragmentSpread
            ast::Selection::FragmentSpread(fragment_spread) => {
                let skip = fragment_spread
                    .directives()
                    .map(|directives| {
                        // skip directives have been validated before, so we're safe here
                        for directive in directives.directives() {
                            if let Some(skip) = parse_skip(&directive) {
                                return skip;
                            }
                        }
                        Skip::No
                    })
                    .unwrap_or(Skip::No);
                if skip.statically_skipped() {
                    return Ok(None);
                }

                let include = fragment_spread
                    .directives()
                    .map(|directives| {
                        for directive in directives.directives() {
                            // include directives have been validated before, so we're safe here
                            if let Some(include) = parse_include(&directive) {
                                return include;
                            }
                        }
                        Include::Yes
                    })
                    .unwrap_or(Include::Yes);
                if include.statically_skipped() {
                    return Ok(None);
                }

                let name = fragment_spread
                    .fragment_name()
                    .expect("the node FragmentName is not optional in the spec; qed")
                    .name()
                    .unwrap()
                    .text()
                    .to_string();

                Some(Self::FragmentSpread {
                    name,
                    known_type: current_type.inner_type_name().map(|s| s.to_string()),
                    skip,
                    include,
                })
            }
        };

        Ok(selection)
    }
}

pub(crate) fn parse_skip(directive: &ast::Directive) -> Option<Skip> {
    if directive
        .name()
        .map(|name| &name.text().to_string() == "skip")
        .unwrap_or(false)
    {
        if let Some(argument) = directive
            .arguments()
            .and_then(|args| args.arguments().next())
        {
            if argument
                .name()
                .map(|name| &name.text().to_string() == "if")
                .unwrap_or(false)
            {
                // invalid argument values should have been already validated
                let res = match argument.value() {
                    Some(Value::BooleanValue(b)) => {
                        match (b.true_token().is_some(), b.false_token().is_some()) {
                            (true, false) => Some(Skip::Yes),
                            (false, true) => Some(Skip::No),
                            _ => None,
                        }
                    }
                    Some(Value::Variable(variable)) => variable
                        .name()
                        .map(|name| Skip::Variable(name.text().to_string())),
                    _ => None,
                };
                return res;
            }
        }
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Skip {
    Yes,
    No,
    Variable(String),
}

impl Skip {
    pub(crate) fn should_skip(&self, variables: &Object) -> Option<bool> {
        match self {
            Skip::Yes => Some(true),
            Skip::No => Some(false),
            Skip::Variable(variable_name) => variables
                .get(variable_name.as_str())
                .and_then(|v| v.as_bool()),
        }
    }
    pub(crate) fn statically_skipped(&self) -> bool {
        matches!(self, Skip::Yes)
    }
}

pub(crate) fn parse_include(directive: &ast::Directive) -> Option<Include> {
    if directive
        .name()
        .map(|name| &name.text().to_string() == "include")
        .unwrap_or(false)
    {
        if let Some(argument) = directive
            .arguments()
            .and_then(|args| args.arguments().next())
        {
            if argument
                .name()
                .map(|name| &name.text().to_string() == "if")
                .unwrap_or(false)
            {
                // invalid argument values should have been already validated
                let res = match argument.value() {
                    Some(Value::BooleanValue(b)) => {
                        match (b.true_token().is_some(), b.false_token().is_some()) {
                            (true, false) => Some(Include::Yes),
                            (false, true) => Some(Include::No),
                            _ => None,
                        }
                    }
                    Some(Value::Variable(variable)) => variable
                        .name()
                        .map(|name| Include::Variable(name.text().to_string())),
                    _ => None,
                };
                return res;
            }
        }
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Include {
    Yes,
    No,
    Variable(String),
}

impl Include {
    pub(crate) fn should_include(&self, variables: &Object) -> Option<bool> {
        match self {
            Include::Yes => Some(true),
            Include::No => Some(false),
            Include::Variable(variable_name) => variables
                .get(variable_name.as_str())
                .and_then(|v| v.as_bool()),
        }
    }
    pub(crate) fn statically_skipped(&self) -> bool {
        matches!(self, Include::No)
    }
}