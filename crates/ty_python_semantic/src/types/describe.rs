//! A structural description of types, for embedders that translate Python types into another
//! type system.
//!
//! The description exposes one level of a type at a time. The types it contains can be
//! described in turn, which lets a caller guard against recursive types.

use ruff_db::files::File;
use ty_python_core::ProgramFile;

use crate::Db;
use crate::place::imported_symbol;
use crate::types::class::ClassType;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::recursive::UnfoldResult;
use crate::types::signatures::ParameterKind;
use crate::types::tuple::{Tuple, TupleSpec};
use crate::types::typed_dict::TypedDictType;
use crate::types::{KnownClass, ProgramEnvironment, Type};

/// One level of a type's structure.
#[derive(Debug)]
pub enum Description<'db> {
    /// A dynamic type, such as `Any` or a type that could not be inferred.
    Dynamic,
    /// The empty type.
    Never,
    /// A type alias, with the alias's name, the file that defines it, and its value.
    Alias {
        name: String,
        file: File,
        value: Type<'db>,
    },
    /// A literal value.
    Literal(Literal),
    /// An instance of a class.
    Instance(Instance<'db>),
    /// A tuple, with fixed elements before and after an optional variable-length segment.
    Tuple {
        prefix: Vec<Type<'db>>,
        variable: Option<Type<'db>>,
        suffix: Vec<Type<'db>>,
    },
    /// A typed dict.
    TypedDict(TypedDict<'db>),
    /// A union of types.
    Union(Vec<Type<'db>>),
    /// A callable with one signature for each of its overloads.
    Callable(Vec<Signature<'db>>),
    /// Any other type.
    Other,
}

/// A literal value.
#[derive(Debug)]
pub enum Literal {
    Bool(bool),
    Int(i64),
    String(String),
    /// Any other literal, such as a bytes or enum literal.
    Other,
}

/// An instance of a class.
#[derive(Debug)]
pub struct Instance<'db> {
    /// The class's name.
    pub name: String,
    /// The file that defines the class, if there is one.
    pub file: Option<File>,
    /// The class's identity, if it is a class that ty knows.
    pub known: Option<KnownClass>,
    /// The type arguments of the class's specialization.
    pub arguments: Vec<Type<'db>>,
}

/// A typed dict.
#[derive(Debug)]
pub struct TypedDict<'db> {
    /// The name of the class that defines the typed dict, if there is one.
    pub name: Option<String>,
    /// The file that defines the typed dict, if there is one.
    pub file: Option<File>,
    /// The fields, in order of their names.
    pub fields: Vec<Field<'db>>,
}

/// A field of a typed dict.
#[derive(Debug)]
pub struct Field<'db> {
    pub name: String,
    pub required: bool,
    pub ty: Type<'db>,
}

/// A signature of a callable.
#[derive(Debug)]
pub struct Signature<'db> {
    pub parameters: Vec<Parameter<'db>>,
    /// The return type, which for an async function is a `CoroutineType` instance.
    pub return_ty: Type<'db>,
}

/// A parameter of a signature.
#[derive(Debug)]
pub struct Parameter<'db> {
    pub name: Option<String>,
    pub kind: Kind,
    pub has_default: bool,
    /// The annotated type, which is dynamic for an unannotated parameter.
    pub ty: Type<'db>,
}

/// The kind of a parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    PositionalOnly,
    PositionalOrKeyword,
    Variadic,
    KeywordOnly,
    KeywordVariadic,
}

/// Get the type of a module's member, as an importing module sees it.
pub fn module_member<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let env = ProgramEnvironment::from_file(file);
    imported_symbol(db, &env, Some(file), name, None)
        .place
        .ignore_possibly_undefined()
}

/// Describe one level of a type's structure, looking through recursive types and type variables.
pub fn describe<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Description<'db> {
    match ty {
        Type::Dynamic(_) | Type::Divergent(_) => Description::Dynamic,
        Type::Never => Description::Never,
        Type::TypeAlias(alias) => Description::Alias {
            name: alias.name(db).to_owned(),
            file: alias.definition(db).file(db),
            value: alias.value_type(db),
        },
        Type::Recursive(recursive) => match recursive.unfold(db, env) {
            UnfoldResult::Unfolded(ty) => describe(db, env, ty),
            UnfoldResult::Unchanged(_) => Description::Other,
        },
        Type::TypeVar(typevar) => match typevar.typevar(db).upper_bound(db, env) {
            Some(bound) => describe(db, env, bound),
            None => Description::Dynamic,
        },
        Type::NewTypeInstance(newtype) => describe(db, env, newtype.concrete_base_type(db)),
        Type::LiteralValue(literal) => {
            let literal = match literal.kind() {
                LiteralValueTypeKind::Bool(value) => Literal::Bool(value),
                LiteralValueTypeKind::Int(value) => Literal::Int(value.as_i64()),
                LiteralValueTypeKind::String(value) => Literal::String(value.value(db).to_owned()),
                _ => Literal::Other,
            };
            Description::Literal(literal)
        }
        Type::Union(union) => Description::Union(union.elements(db).to_vec()),
        Type::NominalInstance(instance) => {
            if let Some(tuple) = instance.tuple_spec(db, env) {
                return tuple_description(&tuple);
            }
            Description::Instance(instance_description(db, instance.class(db, env)))
        }
        Type::TypedDict(typed_dict) => {
            let (name, file) = match typed_dict {
                TypedDictType::Class(class) => (
                    Some(class.name(db).to_string()),
                    class.definition(db).map(|definition| definition.file(db)),
                ),
                TypedDictType::Synthesized(_) => (None, None),
            };
            let fields = typed_dict
                .items(db)
                .iter()
                .map(|(name, field)| Field {
                    name: name.to_string(),
                    required: field.is_required(),
                    ty: field.declared_ty,
                })
                .collect();
            Description::TypedDict(TypedDict { name, file, fields })
        }
        Type::FunctionLiteral(function) => {
            let signature = function.signature_with_inferred_return_type(db);
            Description::Callable(signature.iter().map(signature_description).collect())
        }
        Type::Callable(callable) => {
            let signatures = callable.signatures(db);
            Description::Callable(signatures.iter().map(signature_description).collect())
        }
        _ => Description::Other,
    }
}

fn instance_description<'db>(db: &'db dyn Db, class: ClassType<'db>) -> Instance<'db> {
    let arguments = match class {
        ClassType::NonGeneric(_) => Vec::new(),
        ClassType::Generic(alias) => alias.specialization(db).types(db).to_vec(),
    };
    Instance {
        name: class.name(db).to_string(),
        file: class.definition(db).map(|definition| definition.file(db)),
        known: class.known(db),
        arguments,
    }
}

fn tuple_description<'db>(tuple: &TupleSpec<'db>) -> Description<'db> {
    match tuple {
        Tuple::Fixed(tuple) => Description::Tuple {
            prefix: tuple.all_elements().to_vec(),
            variable: None,
            suffix: Vec::new(),
        },
        Tuple::Variable(tuple) => {
            let Some(variable) = tuple.variable().homogeneous_type() else {
                return Description::Other;
            };
            Description::Tuple {
                prefix: tuple.prefix_elements().to_vec(),
                variable: Some(variable),
                suffix: tuple.suffix_elements().to_vec(),
            }
        }
    }
}

fn signature_description<'db>(
    signature: &crate::types::signatures::Signature<'db>,
) -> Signature<'db> {
    let parameters = signature
        .parameters()
        .iter()
        .map(|parameter| {
            let kind = match parameter.kind() {
                ParameterKind::PositionalOnly { .. } => Kind::PositionalOnly,
                ParameterKind::PositionalOrKeyword { .. } => Kind::PositionalOrKeyword,
                ParameterKind::Variadic { .. } => Kind::Variadic,
                ParameterKind::KeywordOnly { .. } => Kind::KeywordOnly,
                ParameterKind::KeywordVariadic { .. } => Kind::KeywordVariadic,
            };
            Parameter {
                name: parameter.name().map(ToString::to_string),
                kind,
                has_default: parameter.has_default(),
                ty: parameter.annotated_type(),
            }
        })
        .collect();
    Signature {
        parameters,
        return_ty: signature.return_ty,
    }
}
