use std::borrow::{Borrow, Cow};

use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::Parse, parse_quote, punctuated::Punctuated, spanned::Spanned, token::Comma, Attribute,
    Data, Field, Fields, FieldsNamed, FieldsUnnamed, GenericArgument, GenericParam, Generics,
    Lifetime, LifetimeParam, Path, PathArguments, Token, Type, Variant, Visibility,
};

use crate::{
    as_tokens_or_diagnostics,
    component::features::{Example, Rename},
    doc_comment::CommentAttributes,
    Array, Deprecated, Diagnostics, OptionExt, ToTokensDiagnostics,
};

use self::{
    enum_variant::{
        AdjacentlyTaggedEnum, CustomEnum, Enum, ObjectVariant, SimpleEnumVariant, TaggedEnum,
        UntaggedEnum,
    },
    features::{
        ComplexEnumFeatures, EnumFeatures, EnumNamedFieldVariantFeatures,
        EnumUnnamedFieldVariantFeatures, FromAttributes, NamedFieldFeatures,
        NamedFieldStructFeatures, UnnamedFieldStructFeatures,
    },
};

use super::{
    features::{
        parse_features, pop_feature, pop_feature_as_inner, As, Feature, FeaturesExt, IntoInner,
        RenameAll, ToTokensExt,
    },
    serde::{self, SerdeContainer, SerdeEnumRepr, SerdeValue},
    ComponentSchema, FieldRename, FlattenedMapSchema, TypeTree, ValueType, VariantRename,
};

mod enum_variant;
mod features;
pub mod xml;

pub struct Schema<'a> {
    ident: &'a Ident,
    attributes: &'a [Attribute],
    generics: &'a Generics,
    aliases: Option<Punctuated<AliasSchema, Comma>>,
    data: &'a Data,
    vis: &'a Visibility,
}

impl<'a> Schema<'a> {
    const TO_SCHEMA_LIFETIME: &'static str = "'__s";
    pub fn new(
        data: &'a Data,
        attributes: &'a [Attribute],
        ident: &'a Ident,
        generics: &'a Generics,
        vis: &'a Visibility,
    ) -> Result<Self, Diagnostics> {
        let aliases = if generics.type_params().count() > 0 {
            parse_aliases(attributes)?
        } else {
            None
        };

        Ok(Self {
            data,
            ident,
            attributes,
            generics,
            aliases,
            vis,
        })
    }
}

impl ToTokensDiagnostics for Schema<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let ident = self.ident;
        let variant = SchemaVariant::new(
            self.data,
            self.attributes,
            ident,
            self.generics,
            None::<Vec<(TypeTree, &TypeTree)>>,
        )?;

        let (_, ty_generics, where_clause) = self.generics.split_for_impl();

        let life = &Lifetime::new(Schema::TO_SCHEMA_LIFETIME, Span::call_site());

        let schema_ty: Type = parse_quote!(#ident #ty_generics);
        let schema_children = &*TypeTree::from_type(&schema_ty)?
            .children
            .unwrap_or_default();

        let aliases = self.aliases.as_ref().map_try(|aliases| {
            let alias_schemas = aliases
                .iter()
                .map(|alias| {
                    let name = &*alias.name;
                    let alias_type_tree = TypeTree::from_type(&alias.ty);

                    SchemaVariant::new(
                        self.data,
                        self.attributes,
                        ident,
                        self.generics,
                        alias_type_tree?
                            .children
                            .map(|children| children.into_iter().zip(schema_children)),
                    )
                    .and_then(|variant| {
                        let mut alias_tokens = TokenStream::new();
                        match variant.to_tokens(&mut alias_tokens) {
                            Ok(_) => Ok(quote! { (#name, #alias_tokens.into()) }),
                            Err(diagnostics) => Err(diagnostics),
                        }
                    })
                })
                .collect::<Result<Array<TokenStream>, Diagnostics>>()?;

            Result::<TokenStream, Diagnostics>::Ok(quote! {
                fn aliases() -> Vec<(& #life str, utoipa::openapi::schema::Schema)> {
                    #alias_schemas.to_vec()
                }
            })
        })?;

        let type_aliases = self.aliases.as_ref().map_try(|aliases| {
            aliases
                .iter()
                .map(|alias| {
                    let name = quote::format_ident!("{}", alias.name);
                    let ty = &alias.ty;
                    let vis = self.vis;
                    let name_generics = alias.get_lifetimes()?.fold(
                        Punctuated::<&GenericArgument, Comma>::new(),
                        |mut acc, lifetime| {
                            acc.push(lifetime);
                            acc
                        },
                    );

                    Ok(quote! {
                        #vis type #name < #name_generics > = #ty;
                    })
                })
                .collect::<Result<TokenStream, Diagnostics>>()
        })?;

        let name = if let Some(schema_as) = variant.get_schema_as() {
            format_path_ref(&schema_as.0.path)
        } else {
            ident.to_string()
        };

        let schema_lifetime: GenericParam = LifetimeParam::new(life.clone()).into();
        let schema_generics = Generics {
            params: [schema_lifetime.clone()].into_iter().collect(),
            ..Default::default()
        };

        let mut impl_generics = self.generics.clone();
        impl_generics.params.push(schema_lifetime);
        let (impl_generics, _, _) = impl_generics.split_for_impl();

        let mut variant_tokens = TokenStream::new();
        variant.to_tokens(&mut variant_tokens)?;

        tokens.extend(quote! {
            impl #impl_generics utoipa::ToSchema #schema_generics for #ident #ty_generics #where_clause {
                fn schema() -> (& #life str, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>) {
                    (#name, #variant_tokens.into())
                }

                #aliases
            }

            #type_aliases
        });
        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
enum SchemaVariant<'a> {
    Named(NamedStructSchema<'a>),
    Unnamed(UnnamedStructSchema<'a>),
    Enum(EnumSchema<'a>),
    Unit(UnitStructVariant),
}

impl<'a> SchemaVariant<'a> {
    pub fn new<I: IntoIterator<Item = (TypeTree<'a>, &'a TypeTree<'a>)>>(
        data: &'a Data,
        attributes: &'a [Attribute],
        ident: &'a Ident,
        generics: &'a Generics,
        aliases: Option<I>,
    ) -> Result<SchemaVariant<'a>, Diagnostics> {
        match data {
            Data::Struct(content) => match &content.fields {
                Fields::Unnamed(fields) => {
                    let FieldsUnnamed { unnamed, .. } = fields;
                    let mut unnamed_features = attributes
                        .parse_features::<UnnamedFieldStructFeatures>()?
                        .into_inner();

                    let schema_as = pop_feature_as_inner!(unnamed_features => Feature::As(_v));
                    Ok(Self::Unnamed(UnnamedStructSchema {
                        struct_name: Cow::Owned(ident.to_string()),
                        attributes,
                        features: unnamed_features,
                        fields: unnamed,
                        schema_as,
                    }))
                }
                Fields::Named(fields) => {
                    let FieldsNamed { named, .. } = fields;
                    let mut named_features = attributes
                        .parse_features::<NamedFieldStructFeatures>()?
                        .into_inner();
                    let schema_as = pop_feature_as_inner!(named_features => Feature::As(_v));

                    Ok(Self::Named(NamedStructSchema {
                        struct_name: Cow::Owned(ident.to_string()),
                        attributes,
                        rename_all: named_features.pop_rename_all_feature(),
                        features: named_features,
                        fields: named,
                        generics: Some(generics),
                        schema_as,
                        aliases: aliases.map(|aliases| aliases.into_iter().collect()),
                    }))
                }
                Fields::Unit => Ok(Self::Unit(UnitStructVariant)),
            },
            Data::Enum(content) => Ok(Self::Enum(EnumSchema::new(
                Cow::Owned(ident.to_string()),
                &content.variants,
                attributes,
            )?)),
            _ => Err(Diagnostics::with_span(
                ident.span(),
                "unexpected data type, expected syn::Data::Struct or syn::Data::Enum",
            )),
        }
    }

    fn get_schema_as(&self) -> &Option<As> {
        match self {
            Self::Enum(schema) => &schema.schema_as,
            Self::Named(schema) => &schema.schema_as,
            Self::Unnamed(schema) => &schema.schema_as,
            _ => &None,
        }
    }
}

impl ToTokensDiagnostics for SchemaVariant<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        match self {
            Self::Enum(schema) => schema.to_tokens(tokens),
            Self::Named(schema) => schema.to_tokens(tokens),
            Self::Unnamed(schema) => schema.to_tokens(tokens),
            Self::Unit(unit) => {
                unit.to_tokens(tokens);
                Ok(())
            }
        }
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
struct UnitStructVariant;

impl ToTokens for UnitStructVariant {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(quote! {
            utoipa::openapi::schema::empty()
        });
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct NamedStructSchema<'a> {
    pub struct_name: Cow<'a, str>,
    pub fields: &'a Punctuated<Field, Comma>,
    pub attributes: &'a [Attribute],
    pub features: Option<Vec<Feature>>,
    pub rename_all: Option<RenameAll>,
    pub generics: Option<&'a Generics>,
    pub aliases: Option<Vec<(TypeTree<'a>, &'a TypeTree<'a>)>>,
    pub schema_as: Option<As>,
}

#[cfg_attr(feature = "debug", derive(Debug))]
struct NamedStructFieldOptions<'a> {
    property: Property,
    rename_field_value: Option<Cow<'a, str>>,
    required: Option<super::features::Required>,
    is_option: bool,
}

impl NamedStructSchema<'_> {
    fn get_named_struct_field_options(
        &self,
        field: &Field,
        field_rules: &SerdeValue,
        container_rules: &SerdeContainer,
    ) -> Result<NamedStructFieldOptions<'_>, Diagnostics> {
        let type_tree = &mut TypeTree::from_type(&field.ty)?;
        if let Some(aliases) = &self.aliases {
            for (new_generic, old_generic_matcher) in aliases.iter() {
                if let Some(generic_match) = type_tree.find_mut(old_generic_matcher) {
                    *generic_match = new_generic.clone();
                }
            }
        }

        let mut field_features = field
            .attrs
            .parse_features::<NamedFieldFeatures>()?
            .into_inner();

        let schema_default = self
            .features
            .as_ref()
            .map(|features| features.iter().any(|f| matches!(f, Feature::Default(_))))
            .unwrap_or(false);
        let serde_default = container_rules.default;

        if schema_default || serde_default {
            let features_inner = field_features.get_or_insert(vec![]);
            if !features_inner
                .iter()
                .any(|f| matches!(f, Feature::Default(_)))
            {
                let field_ident = field.ident.as_ref().unwrap().to_owned();
                let struct_ident = format_ident!("{}", &self.struct_name);
                features_inner.push(Feature::Default(
                    crate::features::Default::new_default_trait(struct_ident, field_ident.into()),
                ));
            }
        }

        // check for Rust's `#[deprecated]` attribute first, then check for `deprecated` feature
        let deprecated = super::get_deprecated(&field.attrs).or_else(|| {
            pop_feature!(field_features => Feature::Deprecated(_)).and_then(|feature| match feature
            {
                Feature::Deprecated(_) => Some(Deprecated::True),
                _ => None,
            })
        });

        let rename_field =
            pop_feature!(field_features => Feature::Rename(_)).and_then(|feature| match feature {
                Feature::Rename(rename) => Some(Cow::Owned(rename.into_value())),
                _ => None,
            });

        let value_type = field_features
            .as_mut()
            .and_then(|features| features.pop_value_type_feature());
        let override_type_tree = value_type
            .as_ref()
            .map_try(|value_type| value_type.as_type_tree())?;
        let comments = CommentAttributes::from_attributes(&field.attrs);
        let schema_with = pop_feature!(field_features => Feature::SchemaWith(_));
        let required = pop_feature_as_inner!(field_features => Feature::Required(_v));
        let type_tree = override_type_tree.as_ref().unwrap_or(type_tree);
        let is_option = type_tree.is_option();

        Ok(NamedStructFieldOptions {
            property: if let Some(schema_with) = schema_with {
                Property::SchemaWith(schema_with)
            } else {
                let cs = super::ComponentSchemaProps {
                    type_tree,
                    features: field_features,
                    description: Some(&comments),
                    deprecated: deprecated.as_ref(),
                    object_name: self.struct_name.as_ref(),
                };
                if is_flatten(field_rules) && type_tree.is_map() {
                    Property::FlattenedMap(FlattenedMapSchema::new(cs)?)
                } else {
                    Property::Schema(ComponentSchema::new(cs)?)
                }
            },
            rename_field_value: rename_field,
            required,
            is_option,
        })
    }
}

impl ToTokensDiagnostics for NamedStructSchema<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let container_rules = serde::parse_container(self.attributes)?;

        let fields = self
            .fields
            .iter()
            .map(|field| {
                let mut field_name = Cow::Owned(field.ident.as_ref().unwrap().to_string());

                if Borrow::<str>::borrow(&field_name).starts_with("r#") {
                    field_name = Cow::Owned(field_name[2..].to_string());
                }

                let field_rules = serde::parse_value(&field.attrs);
                let field_rules = match field_rules {
                    Ok(field_rules) => field_rules,
                    Err(diagnostics) => return Err(diagnostics),
                };
                let field_options =
                    self.get_named_struct_field_options(field, &field_rules, &container_rules);

                match field_options {
                    Ok(field_options) => Ok((field_options, field_rules, field_name, field)),
                    Err(options_diagnostics) => Err(options_diagnostics),
                }
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?;

        let mut object_tokens = fields
            .iter()
            .filter(|(_, field_rules, ..)| is_not_skipped(field_rules) && !is_flatten(field_rules))
            .map(|(property, field_rules, field_name, field)| {
                Ok((
                    property,
                    field_rules,
                    field_name,
                    field,
                    as_tokens_or_diagnostics!(&property.property),
                ))
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?
            .into_iter()
            .fold(
                quote! { utoipa::openapi::ObjectBuilder::new() },
                |mut object_tokens,
                 (
                    NamedStructFieldOptions {
                        rename_field_value,
                        required,
                        is_option,
                        ..
                    },
                    field_rules,
                    field_name,
                    _field,
                    field_schema,
                )| {
                    let rename_to = field_rules
                        .rename
                        .as_deref()
                        .map(Cow::Borrowed)
                        .or(rename_field_value.as_ref().cloned());
                    let rename_all = container_rules.rename_all.as_ref().or(self
                        .rename_all
                        .as_ref()
                        .map(|rename_all| rename_all.as_rename_rule()));

                    let name =
                        super::rename::<FieldRename>(field_name.borrow(), rename_to, rename_all)
                            .unwrap_or(Cow::Borrowed(field_name.borrow()));

                    object_tokens.extend(quote! {
                        .property(#name, #field_schema)
                    });

                    if (!is_option && super::is_required(field_rules, &container_rules))
                        || required
                            .as_ref()
                            .map(super::features::Required::is_true)
                            .unwrap_or(false)
                    {
                        object_tokens.extend(quote! {
                            .required(#name)
                        })
                    }

                    object_tokens
                },
            );

        let flatten_fields = fields
            .iter()
            .filter(|(_, field_rules, ..)| is_flatten(field_rules))
            .collect::<Vec<_>>();

        let all_of = if !flatten_fields.is_empty() {
            let mut flattened_tokens = TokenStream::new();
            let mut flattened_map_field = None;

            for (options, _, _, field) in flatten_fields {
                let NamedStructFieldOptions { property, .. } = options;
                let property_schema = as_tokens_or_diagnostics!(property);

                match property {
                    Property::Schema(_) | Property::SchemaWith(_) => {
                        flattened_tokens.extend(quote! { .item(#property_schema) })
                    }
                    Property::FlattenedMap(_) => {
                        match flattened_map_field {
                            None => {
                                object_tokens.extend(
                                    quote! { .additional_properties(Some(#property_schema)) },
                                );
                                flattened_map_field = Some(field);
                            }
                            Some(flattened_map_field) => {
                                return Err(Diagnostics::with_span(
                                    self.fields.span(),
                                    format!("The structure `{}` contains multiple flattened map fields.", self.struct_name))
                                    .note(
                                        format!("first flattened map field was declared here as `{}`",
                                        flattened_map_field.ident.as_ref().unwrap()))
                                    .note(format!("second flattened map field was declared here as `{}`", field.ident.as_ref().unwrap()))
                                );
                            }
                        }
                    }
                }
            }

            if flattened_tokens.is_empty() {
                tokens.extend(object_tokens);
                false
            } else {
                tokens.extend(quote! {
                    utoipa::openapi::AllOfBuilder::new()
                        #flattened_tokens
                    .item(#object_tokens)
                });
                true
            }
        } else {
            tokens.extend(object_tokens);
            false
        };

        if !all_of && container_rules.deny_unknown_fields {
            tokens.extend(quote! {
                .additional_properties(Some(utoipa::openapi::schema::AdditionalProperties::FreeForm(false)))
            });
        }

        if let Some(deprecated) = super::get_deprecated(self.attributes) {
            tokens.extend(quote! { .deprecated(Some(#deprecated)) });
        }

        if let Some(struct_features) = self.features.as_ref() {
            tokens.extend(struct_features.to_token_stream()?)
        }

        let description = CommentAttributes::from_attributes(self.attributes).as_formatted_string();
        if !description.is_empty() {
            tokens.extend(quote! {
                .description(Some(#description))
            })
        }

        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
struct UnnamedStructSchema<'a> {
    struct_name: Cow<'a, str>,
    fields: &'a Punctuated<Field, Comma>,
    attributes: &'a [Attribute],
    features: Option<Vec<Feature>>,
    schema_as: Option<As>,
}

impl ToTokensDiagnostics for UnnamedStructSchema<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let fields_len = self.fields.len();
        let first_field = self.fields.first().unwrap();
        let first_part = &TypeTree::from_type(&first_field.ty)?;

        let all_fields_are_same = fields_len == 1
            || self
                .fields
                .iter()
                .skip(1)
                .map(|field| TypeTree::from_type(&field.ty))
                .collect::<Result<Vec<TypeTree>, Diagnostics>>()?
                .iter()
                .all(|schema_part| first_part == schema_part);

        let deprecated = super::get_deprecated(self.attributes);
        if all_fields_are_same {
            let mut unnamed_struct_features = self.features.clone();
            let value_type = unnamed_struct_features
                .as_mut()
                .and_then(|features| features.pop_value_type_feature());
            let override_type_tree = value_type
                .as_ref()
                .map_try(|value_type| value_type.as_type_tree())?;

            if fields_len == 1 {
                if let Some(ref mut features) = unnamed_struct_features {
                    if pop_feature!(features => Feature::Default(crate::features::Default(None)))
                        .is_some()
                    {
                        let struct_ident = format_ident!("{}", &self.struct_name);
                        let index: syn::Index = 0.into();
                        features.push(Feature::Default(
                            crate::features::Default::new_default_trait(struct_ident, index.into()),
                        ));
                    }
                }
            }

            tokens.extend(
                ComponentSchema::new(super::ComponentSchemaProps {
                    type_tree: override_type_tree.as_ref().unwrap_or(first_part),
                    features: unnamed_struct_features,
                    description: Some(&CommentAttributes::from_attributes(self.attributes)),
                    deprecated: deprecated.as_ref(),
                    object_name: self.struct_name.as_ref(),
                })?
                .to_token_stream(),
            );
        } else {
            // Struct that has multiple unnamed fields is serialized to array by default with serde.
            // See: https://serde.rs/json.html
            // Typically OpenAPI does not support multi type arrays thus we simply consider the case
            // as generic object array
            tokens.extend(quote! {
                utoipa::openapi::ObjectBuilder::new()
            });

            if let Some(deprecated) = deprecated {
                tokens.extend(quote! { .deprecated(Some(#deprecated)) });
            }

            if let Some(ref attrs) = self.features {
                tokens.extend(attrs.to_token_stream()?)
            }
        }

        if fields_len > 1 {
            let description =
                CommentAttributes::from_attributes(self.attributes).as_formatted_string();
            tokens.extend(
                quote! { .to_array_builder().description(Some(#description)).max_items(Some(#fields_len)).min_items(Some(#fields_len)) },
            )
        }

        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct EnumSchema<'a> {
    schema_type: EnumSchemaType<'a>,
    schema_as: Option<As>,
}

impl<'e> EnumSchema<'e> {
    pub fn new(
        enum_name: Cow<'e, str>,
        variants: &'e Punctuated<Variant, Comma>,
        attributes: &'e [Attribute],
    ) -> Result<Self, Diagnostics> {
        if variants
            .iter()
            .all(|variant| matches!(variant.fields, Fields::Unit))
        {
            #[cfg(feature = "repr")]
            {
                let repr_enum = attributes
                    .iter()
                    .find_map(|attribute| {
                        if attribute.path().is_ident("repr") {
                            attribute.parse_args::<syn::TypePath>().ok()
                        } else {
                            None
                        }
                    })
                    .map_try(|enum_type| {
                        let mut repr_enum_features =
                            features::parse_schema_features_with(attributes, |input| {
                                Ok(parse_features!(
                                    input as super::features::Example,
                                    super::features::Default,
                                    super::features::Title,
                                    As
                                ))
                            })?
                            .unwrap_or_default();

                        let schema_as =
                            pop_feature_as_inner!(repr_enum_features => Feature::As(_v));
                        Result::<EnumSchema, Diagnostics>::Ok(Self {
                            schema_type: EnumSchemaType::Repr(ReprEnum {
                                variants,
                                attributes,
                                enum_type,
                                enum_features: repr_enum_features,
                            }),
                            schema_as,
                        })
                    })?;

                match repr_enum {
                    Some(repr) => Ok(repr),
                    None => {
                        let mut simple_enum_features = attributes
                            .parse_features::<EnumFeatures>()?
                            .into_inner()
                            .unwrap_or_default();
                        let schema_as =
                            pop_feature_as_inner!(simple_enum_features => Feature::As(_v));
                        let rename_all = simple_enum_features.pop_rename_all_feature();

                        Ok(Self {
                            schema_type: EnumSchemaType::Simple(SimpleEnum {
                                attributes,
                                variants,
                                enum_features: simple_enum_features,
                                rename_all,
                            }),
                            schema_as,
                        })
                    }
                }
            }

            #[cfg(not(feature = "repr"))]
            {
                let mut simple_enum_features = attributes
                    .parse_features::<EnumFeatures>()?
                    .into_inner()
                    .unwrap_or_default();
                let schema_as = pop_feature_as_inner!(simple_enum_features => Feature::As(_v));
                let rename_all = simple_enum_features.pop_rename_all_feature();

                Ok(Self {
                    schema_type: EnumSchemaType::Simple(SimpleEnum {
                        attributes,
                        variants,
                        enum_features: simple_enum_features,
                        rename_all,
                    }),
                    schema_as,
                })
            }
        } else {
            let mut enum_features = attributes
                .parse_features::<ComplexEnumFeatures>()?
                .into_inner()
                .unwrap_or_default();
            let schema_as = pop_feature_as_inner!(enum_features => Feature::As(_v));
            let rename_all = enum_features.pop_rename_all_feature();

            Ok(Self {
                schema_type: EnumSchemaType::Complex(ComplexEnum {
                    enum_name,
                    attributes,
                    variants,
                    rename_all,
                    enum_features,
                }),
                schema_as,
            })
        }
    }
}

impl ToTokensDiagnostics for EnumSchema<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        self.schema_type.to_tokens(tokens)
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
enum EnumSchemaType<'e> {
    Simple(SimpleEnum<'e>),
    #[cfg(feature = "repr")]
    Repr(ReprEnum<'e>),
    Complex(ComplexEnum<'e>),
}

impl ToTokensDiagnostics for EnumSchemaType<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let attributes = match self {
            Self::Simple(simple) => {
                ToTokensDiagnostics::to_tokens(simple, tokens)?;
                simple.attributes
            }
            #[cfg(feature = "repr")]
            Self::Repr(repr) => {
                ToTokensDiagnostics::to_tokens(repr, tokens)?;
                repr.attributes
            }
            Self::Complex(complex) => {
                ToTokensDiagnostics::to_tokens(complex, tokens)?;
                complex.attributes
            }
        };

        if let Some(deprecated) = super::get_deprecated(attributes) {
            tokens.extend(quote! { .deprecated(Some(#deprecated)) });
        }

        let description = CommentAttributes::from_attributes(attributes).as_formatted_string();
        if !description.is_empty() {
            tokens.extend(quote! {
                .description(Some(#description))
            })
        }

        Ok(())
    }
}

#[cfg(feature = "repr")]
#[cfg_attr(feature = "debug", derive(Debug))]
struct ReprEnum<'a> {
    variants: &'a Punctuated<Variant, Comma>,
    attributes: &'a [Attribute],
    enum_type: syn::TypePath,
    enum_features: Vec<Feature>,
}

#[cfg(feature = "repr")]
impl ToTokensDiagnostics for ReprEnum<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let container_rules = serde::parse_container(self.attributes)?;
        let enum_variants = self
            .variants
            .iter()
            .map(|variant| match serde::parse_value(&variant.attrs) {
                Ok(variant_rules) => Ok((variant, variant_rules)),
                Err(diagnostics) => Err(diagnostics),
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?
            .into_iter()
            .filter_map(|(variant, variant_rules)| {
                let variant_type = &variant.ident;

                if is_not_skipped(&variant_rules) {
                    let repr_type = &self.enum_type;
                    Some(enum_variant::ReprVariant {
                        value: quote! { Self::#variant_type as #repr_type },
                        type_path: repr_type,
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<enum_variant::ReprVariant<TokenStream>>>();

        regular_enum_to_tokens(
            tokens,
            &container_rules,
            self.enum_features.to_token_stream()?,
            || enum_variants,
        );

        Ok(())
    }
}

fn rename_enum_variant<'a>(
    name: &'a str,
    features: &mut Vec<Feature>,
    variant_rules: &'a SerdeValue,
    container_rules: &'a SerdeContainer,
    rename_all: &'a Option<RenameAll>,
) -> Option<Cow<'a, str>> {
    let rename = features
        .pop_rename_feature()
        .map(|rename| rename.into_value());
    let rename_to = variant_rules
        .rename
        .as_deref()
        .map(Cow::Borrowed)
        .or(rename.map(Cow::Owned));

    let rename_all = container_rules.rename_all.as_ref().or(rename_all
        .as_ref()
        .map(|rename_all| rename_all.as_rename_rule()));

    super::rename::<VariantRename>(name, rename_to, rename_all)
}

#[cfg_attr(feature = "debug", derive(Debug))]
struct SimpleEnum<'a> {
    variants: &'a Punctuated<Variant, Comma>,
    attributes: &'a [Attribute],
    enum_features: Vec<Feature>,
    rename_all: Option<RenameAll>,
}

impl ToTokensDiagnostics for SimpleEnum<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let container_rules = serde::parse_container(self.attributes)?;
        let simple_enum_variant = self
            .variants
            .iter()
            .map(|variant| match serde::parse_value(&variant.attrs) {
                Ok(variant_rules) => Ok((variant, variant_rules)),
                Err(diagnostics) => Err(diagnostics),
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?
            .into_iter()
            .filter_map(|(variant, variant_rules)| {
                if is_not_skipped(&variant_rules) {
                    Some((variant, variant_rules))
                } else {
                    None
                }
            })
            .map(|(variant, variant_rules)| {
                let variant_features =
                    features::parse_schema_features_with(&variant.attrs, |input| {
                        Ok(parse_features!(input as Rename))
                    });

                match variant_features {
                    Ok(variant_features) => {
                        Ok((variant, variant_rules, variant_features.unwrap_or_default()))
                    }
                    Err(diagnostics) => Err(diagnostics),
                }
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?
            .into_iter()
            .flat_map(|(variant, variant_rules, mut variant_features)| {
                let name = &*variant.ident.to_string();
                let variant_name = rename_enum_variant(
                    name,
                    &mut variant_features,
                    &variant_rules,
                    &container_rules,
                    &self.rename_all,
                );

                variant_name
                    .map(|name| SimpleEnumVariant {
                        value: name.to_token_stream(),
                    })
                    .or_else(|| {
                        Some(SimpleEnumVariant {
                            value: name.to_token_stream(),
                        })
                    })
            })
            .collect::<Vec<SimpleEnumVariant<TokenStream>>>();

        regular_enum_to_tokens(
            tokens,
            &container_rules,
            self.enum_features.to_token_stream()?,
            || simple_enum_variant,
        );

        Ok(())
    }
}

fn regular_enum_to_tokens<T: self::enum_variant::Variant>(
    tokens: &mut TokenStream,
    container_rules: &SerdeContainer,
    enum_variant_features: TokenStream,
    get_variants_tokens_vec: impl FnOnce() -> Vec<T>,
) {
    let enum_values = get_variants_tokens_vec();

    tokens.extend(match &container_rules.enum_repr {
        SerdeEnumRepr::ExternallyTagged => Enum::new(enum_values).to_token_stream(),
        SerdeEnumRepr::InternallyTagged { tag } => TaggedEnum::new(
            enum_values
                .into_iter()
                .map(|variant| (Cow::Borrowed(tag.as_str()), variant)),
        )
        .to_token_stream(),
        SerdeEnumRepr::Untagged => UntaggedEnum::new().to_token_stream(),
        SerdeEnumRepr::AdjacentlyTagged { tag, content } => {
            AdjacentlyTaggedEnum::new(enum_values.into_iter().map(|variant| {
                (
                    Cow::Borrowed(tag.as_str()),
                    Cow::Borrowed(content.as_str()),
                    variant,
                )
            }))
            .to_token_stream()
        }
        // This should not be possible as serde should not let that happen
        SerdeEnumRepr::UnfinishedAdjacentlyTagged { .. } => panic!("Invalid serde enum repr"),
    });

    tokens.extend(enum_variant_features);
}

#[cfg_attr(feature = "debug", derive(Debug))]
struct ComplexEnum<'a> {
    variants: &'a Punctuated<Variant, Comma>,
    attributes: &'a [Attribute],
    enum_name: Cow<'a, str>,
    enum_features: Vec<Feature>,
    rename_all: Option<RenameAll>,
}

impl ComplexEnum<'_> {
    /// Produce tokens that represent a variant of a [`ComplexEnum`].
    fn variant_tokens(
        &self,
        name: Cow<'_, str>,
        variant: &Variant,
        variant_rules: &SerdeValue,
        container_rules: &SerdeContainer,
        rename_all: &Option<RenameAll>,
    ) -> Result<TokenStream, Diagnostics> {
        // TODO need to be able to split variant.attrs for variant and the struct representation!
        match &variant.fields {
            Fields::Named(named_fields) => {
                let (title_features, mut named_struct_features) = variant
                    .attrs
                    .parse_features::<EnumNamedFieldVariantFeatures>()?
                    .into_inner()
                    .map(|features| features.split_for_title())
                    .unwrap_or_default();
                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut named_struct_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                let example = pop_feature!(named_struct_features => Feature::Example(_));

                Ok(self::enum_variant::Variant::to_tokens(&ObjectVariant {
                    name: variant_name.unwrap_or(Cow::Borrowed(&name)),
                    title: title_features
                        .first()
                        .map(ToTokensDiagnostics::to_token_stream),
                    example: example.as_ref().map(ToTokensDiagnostics::to_token_stream),
                    item: as_tokens_or_diagnostics!(&NamedStructSchema {
                        struct_name: Cow::Borrowed(&*self.enum_name),
                        attributes: &variant.attrs,
                        rename_all: named_struct_features.pop_rename_all_feature(),
                        features: Some(named_struct_features),
                        fields: &named_fields.named,
                        generics: None,
                        aliases: None,
                        schema_as: None,
                    }),
                }))
            }
            Fields::Unnamed(unnamed_fields) => {
                let (title_features, mut unnamed_struct_features) = variant
                    .attrs
                    .parse_features::<EnumUnnamedFieldVariantFeatures>()?
                    .into_inner()
                    .map(|features| features.split_for_title())
                    .unwrap_or_default();
                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut unnamed_struct_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                let example = pop_feature!(unnamed_struct_features => Feature::Example(_));

                Ok(self::enum_variant::Variant::to_tokens(&ObjectVariant {
                    name: variant_name.unwrap_or(Cow::Borrowed(&name)),
                    title: title_features
                        .first()
                        .map(ToTokensDiagnostics::to_token_stream),
                    example: example.as_ref().map(ToTokensDiagnostics::to_token_stream),
                    item: as_tokens_or_diagnostics!(&UnnamedStructSchema {
                        struct_name: Cow::Borrowed(&*self.enum_name),
                        attributes: &variant.attrs,
                        features: Some(unnamed_struct_features),
                        fields: &unnamed_fields.unnamed,
                        schema_as: None,
                    }),
                }))
            }
            Fields::Unit => {
                let mut unit_features =
                    features::parse_schema_features_with(&variant.attrs, |input| {
                        Ok(parse_features!(
                            input as super::features::Title,
                            RenameAll,
                            Rename,
                            Example
                        ))
                    })?
                    .unwrap_or_default();
                let title = pop_feature!(unit_features => Feature::Title(_));
                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut unit_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                let example: Option<Feature> = pop_feature!(unit_features => Feature::Example(_));

                let description =
                    CommentAttributes::from_attributes(&variant.attrs).as_formatted_string();
                let description =
                    (!description.is_empty()).then(|| Feature::Description(description.into()));

                // Unit variant is just simple enum with single variant.
                Ok(Enum::new([SimpleEnumVariant {
                    value: variant_name
                        .unwrap_or(Cow::Borrowed(&name))
                        .to_token_stream(),
                }])
                .with_title(title.as_ref().map(ToTokensDiagnostics::to_token_stream))
                .with_example(example.as_ref().map(ToTokensDiagnostics::to_token_stream))
                .with_description(
                    description
                        .as_ref()
                        .map(ToTokensDiagnostics::to_token_stream),
                )
                .to_token_stream())
            }
        }
    }

    /// Produce tokens that represent a variant of a [`ComplexEnum`] where serde enum attribute
    /// `untagged` applies.
    fn untagged_variant_tokens(&self, variant: &Variant) -> Result<TokenStream, Diagnostics> {
        match &variant.fields {
            Fields::Named(named_fields) => {
                let mut named_struct_features = variant
                    .attrs
                    .parse_features::<EnumNamedFieldVariantFeatures>()?
                    .into_inner()
                    .unwrap_or_default();

                Ok(as_tokens_or_diagnostics!(&NamedStructSchema {
                    struct_name: Cow::Borrowed(&*self.enum_name),
                    attributes: &variant.attrs,
                    rename_all: named_struct_features.pop_rename_all_feature(),
                    features: Some(named_struct_features),
                    fields: &named_fields.named,
                    generics: None,
                    aliases: None,
                    schema_as: None,
                }))
            }
            Fields::Unnamed(unnamed_fields) => {
                let unnamed_struct_features = variant
                    .attrs
                    .parse_features::<EnumUnnamedFieldVariantFeatures>()?
                    .into_inner()
                    .unwrap_or_default();

                Ok(as_tokens_or_diagnostics!(&UnnamedStructSchema {
                    struct_name: Cow::Borrowed(&*self.enum_name),
                    attributes: &variant.attrs,
                    features: Some(unnamed_struct_features),
                    fields: &unnamed_fields.unnamed,
                    schema_as: None,
                }))
            }
            Fields::Unit => {
                let mut unit_features =
                    features::parse_schema_features_with(&variant.attrs, |input| {
                        Ok(parse_features!(input as super::features::Title))
                    })
                    .unwrap_or_default();
                let title = pop_feature!(unit_features => Feature::Title(_));

                Ok(as_tokens_or_diagnostics!(&UntaggedEnum::with_title(title)))
            }
        }
    }

    /// Produce tokens that represent a variant of a [`ComplexEnum`] where serde enum attribute
    /// `tag = ` applies.
    fn tagged_variant_tokens(
        &self,
        tag: &str,
        name: Cow<'_, str>,
        variant: &Variant,
        variant_rules: &SerdeValue,
        container_rules: &SerdeContainer,
        rename_all: &Option<RenameAll>,
    ) -> Result<TokenStream, Diagnostics> {
        match &variant.fields {
            Fields::Named(named_fields) => {
                let (title_features, mut named_struct_features) = variant
                    .attrs
                    .parse_features::<EnumNamedFieldVariantFeatures>()?
                    .into_inner()
                    .map(|features| features.split_for_title())
                    .unwrap_or_default();
                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut named_struct_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                let named_enum = NamedStructSchema {
                    struct_name: Cow::Borrowed(&*self.enum_name),
                    attributes: &variant.attrs,
                    rename_all: named_struct_features.pop_rename_all_feature(),
                    features: Some(named_struct_features),
                    fields: &named_fields.named,
                    generics: None,
                    aliases: None,
                    schema_as: None,
                };
                let named_enum_tokens = as_tokens_or_diagnostics!(&named_enum);
                let title = title_features
                    .first()
                    .map(ToTokensDiagnostics::to_token_stream);

                let variant_name_tokens = Enum::new([SimpleEnumVariant {
                    value: variant_name
                        .unwrap_or(Cow::Borrowed(&name))
                        .to_token_stream(),
                }]);
                Ok(quote! {
                    #named_enum_tokens
                        #title
                        .property(#tag, #variant_name_tokens)
                        .required(#tag)
                })
            }
            Fields::Unnamed(unnamed_fields) => {
                if unnamed_fields.unnamed.len() == 1 {
                    let (title_features, mut unnamed_struct_features) = variant
                        .attrs
                        .parse_features::<EnumUnnamedFieldVariantFeatures>()?
                        .into_inner()
                        .map(|features| features.split_for_title())
                        .unwrap_or_default();
                    let variant_name = rename_enum_variant(
                        name.as_ref(),
                        &mut unnamed_struct_features,
                        variant_rules,
                        container_rules,
                        rename_all,
                    );

                    let unnamed_enum = UnnamedStructSchema {
                        struct_name: Cow::Borrowed(&*self.enum_name),
                        attributes: &variant.attrs,
                        features: Some(unnamed_struct_features),
                        fields: &unnamed_fields.unnamed,
                        schema_as: None,
                    };
                    let unnamed_enum_tokens = as_tokens_or_diagnostics!(&unnamed_enum);

                    let title = title_features
                        .first()
                        .map(ToTokensDiagnostics::to_token_stream);
                    let variant_name_tokens = Enum::new([SimpleEnumVariant {
                        value: variant_name
                            .unwrap_or(Cow::Borrowed(&name))
                            .to_token_stream(),
                    }]);

                    let is_reference = unnamed_fields
                        .unnamed
                        .iter()
                        .map(|field| TypeTree::from_type(&field.ty))
                        .collect::<Result<Vec<TypeTree>, Diagnostics>>()?
                        .iter()
                        .any(|type_tree| type_tree.value_type == ValueType::Object);

                    if is_reference {
                        Ok(quote! {
                            utoipa::openapi::schema::AllOfBuilder::new()
                                #title
                                .item(#unnamed_enum_tokens)
                                .item(utoipa::openapi::schema::ObjectBuilder::new()
                                    .schema_type(utoipa::openapi::schema::SchemaType::Object)
                                    .property(#tag, #variant_name_tokens)
                                    .required(#tag)
                                )
                        })
                    } else {
                        Ok(quote! {
                            #unnamed_enum_tokens
                                #title
                                .schema_type(utoipa::openapi::schema::SchemaType::Object)
                                .property(#tag, #variant_name_tokens)
                                .required(#tag)
                        })
                    }
                } else {
                    Err(Diagnostics::with_span(variant.span(),
                        "Unnamed (tuple) enum variants are unsupported for internally tagged enums using the `tag = ` serde attribute")
                        .help("Try using a different serde enum representation")
                        .note("See more about enum limitations here: `https://serde.rs/enum-representations.html#internally-tagged`")
                    )
                }
            }
            Fields::Unit => {
                let mut unit_features =
                    features::parse_schema_features_with(&variant.attrs, |input| {
                        Ok(parse_features!(input as super::features::Title, Rename))
                    })?
                    .unwrap_or_default();
                let title = pop_feature!(unit_features => Feature::Title(_));
                let title_tokens = as_tokens_or_diagnostics!(&title);

                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut unit_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                // Unit variant is just simple enum with single variant.
                let variant_tokens = Enum::new([SimpleEnumVariant {
                    value: variant_name
                        .unwrap_or(Cow::Borrowed(&name))
                        .to_token_stream(),
                }]);

                Ok(quote! {
                    utoipa::openapi::schema::ObjectBuilder::new()
                        #title_tokens
                        .property(#tag, #variant_tokens)
                        .required(#tag)
                })
            }
        }
    }

    // FIXME perhaps design this better to lessen the amount of args.
    #[allow(clippy::too_many_arguments)]
    fn adjacently_tagged_variant_tokens(
        &self,
        tag: &str,
        content: &str,
        name: Cow<'_, str>,
        variant: &Variant,
        variant_rules: &SerdeValue,
        container_rules: &SerdeContainer,
        rename_all: &Option<RenameAll>,
    ) -> Result<TokenStream, Diagnostics> {
        match &variant.fields {
            Fields::Named(named_fields) => {
                let (title_features, mut named_struct_features) = variant
                    .attrs
                    .parse_features::<EnumNamedFieldVariantFeatures>()?
                    .into_inner()
                    .map(|features| features.split_for_title())
                    .unwrap_or_default();
                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut named_struct_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                let named_enum = NamedStructSchema {
                    struct_name: Cow::Borrowed(&*self.enum_name),
                    attributes: &variant.attrs,
                    rename_all: named_struct_features.pop_rename_all_feature(),
                    features: Some(named_struct_features),
                    fields: &named_fields.named,
                    generics: None,
                    aliases: None,
                    schema_as: None,
                };
                let named_enum_tokens = as_tokens_or_diagnostics!(&named_enum);
                let title = title_features
                    .first()
                    .map(ToTokensDiagnostics::to_token_stream);

                let variant_name_tokens = Enum::new([SimpleEnumVariant {
                    value: variant_name
                        .unwrap_or(Cow::Borrowed(&name))
                        .to_token_stream(),
                }]);
                Ok(quote! {
                    utoipa::openapi::schema::ObjectBuilder::new()
                        #title
                        .schema_type(utoipa::openapi::schema::SchemaType::Object)
                        .property(#tag, #variant_name_tokens)
                        .required(#tag)
                        .property(#content, #named_enum_tokens)
                        .required(#content)
                })
            }
            Fields::Unnamed(unnamed_fields) => {
                if unnamed_fields.unnamed.len() == 1 {
                    let (title_features, mut unnamed_struct_features) = variant
                        .attrs
                        .parse_features::<EnumUnnamedFieldVariantFeatures>()?
                        .into_inner()
                        .map(|features| features.split_for_title())
                        .unwrap_or_default();
                    let variant_name = rename_enum_variant(
                        name.as_ref(),
                        &mut unnamed_struct_features,
                        variant_rules,
                        container_rules,
                        rename_all,
                    );

                    let unnamed_enum = UnnamedStructSchema {
                        struct_name: Cow::Borrowed(&*self.enum_name),
                        attributes: &variant.attrs,
                        features: Some(unnamed_struct_features),
                        fields: &unnamed_fields.unnamed,
                        schema_as: None,
                    };
                    let unnamed_enum_tokens = as_tokens_or_diagnostics!(&unnamed_enum);

                    let title = title_features
                        .first()
                        .map(ToTokensDiagnostics::to_token_stream);
                    let variant_name_tokens = Enum::new([SimpleEnumVariant {
                        value: variant_name
                            .unwrap_or(Cow::Borrowed(&name))
                            .to_token_stream(),
                    }]);

                    Ok(quote! {
                        utoipa::openapi::schema::ObjectBuilder::new()
                            #title
                            .schema_type(utoipa::openapi::schema::SchemaType::Object)
                            .property(#tag, #variant_name_tokens)
                            .required(#tag)
                            .property(#content, #unnamed_enum_tokens)
                            .required(#content)
                    })
                } else {
                    Err(
                        Diagnostics::with_span(variant.span(),
                            "Unnamed (tuple) enum variants are unsupported for adjacently tagged enums using the `tag = <tag>, content = <content>` serde attribute")
                            .help("Try using a different serde enum representation")
                            .note("See more about enum limitations here: `https://serde.rs/enum-representations.html#adjacently-tagged`")
                    )
                }
            }
            Fields::Unit => {
                // In this case `content` is simply ignored - there is nothing to put in it.

                let mut unit_features =
                    features::parse_schema_features_with(&variant.attrs, |input| {
                        Ok(parse_features!(input as super::features::Title, Rename))
                    })?
                    .unwrap_or_default();
                let title = pop_feature!(unit_features => Feature::Title(_));
                let title_tokens = as_tokens_or_diagnostics!(&title);

                let variant_name = rename_enum_variant(
                    name.as_ref(),
                    &mut unit_features,
                    variant_rules,
                    container_rules,
                    rename_all,
                );

                // Unit variant is just simple enum with single variant.
                let variant_tokens = Enum::new([SimpleEnumVariant {
                    value: variant_name
                        .unwrap_or(Cow::Borrowed(&name))
                        .to_token_stream(),
                }]);

                Ok(quote! {
                    utoipa::openapi::schema::ObjectBuilder::new()
                        #title_tokens
                        .property(#tag, #variant_tokens)
                        .required(#tag)
                })
            }
        }
    }
}

impl ToTokensDiagnostics for ComplexEnum<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        let attributes = &self.attributes;
        let container_rules = serde::parse_container(attributes)?;

        let enum_repr = &container_rules.enum_repr;
        let tag = match &enum_repr {
            SerdeEnumRepr::AdjacentlyTagged { tag, .. }
            | SerdeEnumRepr::InternallyTagged { tag } => Some(tag),
            SerdeEnumRepr::ExternallyTagged
            | SerdeEnumRepr::Untagged
            | SerdeEnumRepr::UnfinishedAdjacentlyTagged { .. } => None,
        };

        self.variants
            .iter()
            .map(|variant| match serde::parse_value(&variant.attrs) {
                Ok(variant_rules) => Ok((variant, variant_rules)),
                Err(diagnostics) => Err(diagnostics),
            })
            .collect::<Result<Vec<_>, Diagnostics>>()?
            .into_iter()
            .filter_map(|(variant, variant_rules)| {
                if is_not_skipped(&variant_rules) {
                    Some((variant, variant_rules))
                } else {
                    None
                }
            })
            .map(|(variant, variant_serde_rules)| {
                let variant_name = &*variant.ident.to_string();

                match &enum_repr {
                    SerdeEnumRepr::ExternallyTagged => self.variant_tokens(
                        Cow::Borrowed(variant_name),
                        variant,
                        &variant_serde_rules,
                        &container_rules,
                        &self.rename_all,
                    ),
                    SerdeEnumRepr::InternallyTagged { tag } => self.tagged_variant_tokens(
                        tag,
                        Cow::Borrowed(variant_name),
                        variant,
                        &variant_serde_rules,
                        &container_rules,
                        &self.rename_all,
                    ),
                    SerdeEnumRepr::Untagged => self.untagged_variant_tokens(variant),
                    SerdeEnumRepr::AdjacentlyTagged { tag, content } => self
                        .adjacently_tagged_variant_tokens(
                            tag,
                            content,
                            Cow::Borrowed(variant_name),
                            variant,
                            &variant_serde_rules,
                            &container_rules,
                            &self.rename_all,
                        ),
                    SerdeEnumRepr::UnfinishedAdjacentlyTagged { .. } => {
                        unreachable!("Serde should not have parsed an UnfinishedAdjacentlyTagged")
                    }
                }
            })
            .collect::<Result<CustomEnum<'_, TokenStream>, Diagnostics>>()?
            .with_discriminator(tag.map(|t| Cow::Borrowed(t.as_str())))
            .to_tokens(tokens);

        tokens.extend(self.enum_features.to_token_stream()?);
        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
enum Property {
    Schema(ComponentSchema),
    SchemaWith(Feature),
    FlattenedMap(FlattenedMapSchema),
}

impl ToTokensDiagnostics for Property {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        match self {
            Self::Schema(schema) => schema.to_tokens(tokens)?,
            Self::FlattenedMap(schema) => schema.to_tokens(tokens)?,
            Self::SchemaWith(schema_with) => schema_with.to_tokens(tokens)?,
        }
        Ok(())
    }
}

trait SchemaFeatureExt {
    fn split_for_title(self) -> (Vec<Feature>, Vec<Feature>);
}

impl SchemaFeatureExt for Vec<Feature> {
    fn split_for_title(self) -> (Vec<Feature>, Vec<Feature>) {
        self.into_iter()
            .partition(|feature| matches!(feature, Feature::Title(_)))
    }
}

/// Reformat a path reference string that was generated using [`quote`] to be used as a nice compact schema reference,
/// by removing spaces between colon punctuation and `::` and the path segments.
pub(crate) fn format_path_ref(path: &Path) -> String {
    let mut path = path.clone();

    // Generics and path arguments are unsupported
    if let Some(last_segment) = path.segments.last_mut() {
        last_segment.arguments = PathArguments::None;
    }
    // :: are not officially supported in the spec
    // See: https://github.com/juhaku/utoipa/pull/187#issuecomment-1173101405
    path.to_token_stream().to_string().replace(" :: ", ".")
}

#[inline]
fn is_not_skipped(rule: &SerdeValue) -> bool {
    !rule.skip
}

#[inline]
fn is_flatten(rule: &SerdeValue) -> bool {
    rule.flatten
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct AliasSchema {
    pub name: String,
    pub ty: Type,
}

impl AliasSchema {
    fn get_lifetimes(&self) -> Result<impl Iterator<Item = &GenericArgument>, Diagnostics> {
        fn lifetimes_from_type(
            ty: &Type,
        ) -> Result<impl Iterator<Item = &GenericArgument>, Diagnostics> {
            match ty {
                Type::Path(type_path) => Ok(type_path
                    .path
                    .segments
                    .iter()
                    .flat_map(|segment| match &segment.arguments {
                        PathArguments::AngleBracketed(angle_bracketed_args) => {
                            Some(angle_bracketed_args.args.iter())
                        }
                        _ => None,
                    })
                    .flatten()
                    .flat_map(|arg| match arg {
                        GenericArgument::Type(type_argument) => {
                            lifetimes_from_type(type_argument).map(|iter| iter.collect::<Vec<_>>())
                        }
                        _ => Ok(vec![arg]),
                    })
                    .flat_map(|args| args.into_iter().filter(|generic_arg| matches!(generic_arg, syn::GenericArgument::Lifetime(lifetime) if lifetime.ident != "'static"))),
                    ),
                _ => Err(Diagnostics::with_span(ty.span(), "AliasSchema `get_lifetimes` only supports syn::TypePath types"))
            }
        }

        lifetimes_from_type(&self.ty)
    }
}

impl Parse for AliasSchema {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let name = input.parse::<Ident>()?;
        input.parse::<Token![=]>()?;

        Ok(Self {
            name: name.to_string(),
            ty: input.parse::<Type>()?,
        })
    }
}

fn parse_aliases(
    attributes: &[Attribute],
) -> Result<Option<Punctuated<AliasSchema, Comma>>, Diagnostics> {
    attributes
        .iter()
        .find(|attribute| attribute.path().is_ident("aliases"))
        .map_try(|aliases| {
            aliases.parse_args_with(Punctuated::<AliasSchema, Comma>::parse_terminated)
        })
        .map_err(Into::into)
}
