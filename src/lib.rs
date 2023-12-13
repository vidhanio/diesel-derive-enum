#![recursion_limit = "1024"]

extern crate proc_macro;

use heck::{ToKebabCase, ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::quote;
use syn::*;

/// Implement the traits necessary for inserting the enum directly into a database
///
/// # Attributes
///
/// ## Type attributes
///
/// * `#[ExistingTypePath = "crate::schema::sql_types::NewEnum"]` specifies
///   the path to a corresponding diesel type that was already created by the
///   diesel CLI. If omitted, the type will be generated by this macro.
///   *Note*: Only applies to `postgres`, will error if specified for other databases
/// * `#[DieselType = "NewEnumMapping"]` specifies the name for the diesel type
///   to create. If omitted, uses `<enum name>Mapping`.
///   *Note*: Cannot be specified alongside `ExistingTypePath`
/// * `#[DbValueStyle = "snake_case"]` specifies a renaming style from each of
///   the rust enum variants to each of the database variants. Either `camelCase`,
///   `kebab-case`, `PascalCase`, `SCREAMING_SNAKE_CASE`, `snake_case`,
///   `verbatim`. If omitted, uses `snake_case`.
///
/// ## Variant attributes
///
/// * `#[db_rename = "variant"]` specifies the db name for a specific variant.
#[proc_macro_derive(
    DbEnum,
    attributes(PgType, DieselType, ExistingTypePath, DbValueStyle, db_rename)
)]
pub fn derive(input: TokenStream) -> TokenStream {
    let input: DeriveInput = parse_macro_input!(input as DeriveInput);

    let existing_mapping_path = val_from_attrs(&input.attrs, "ExistingTypePath");
    if !cfg!(feature = "postgres") && existing_mapping_path.is_some() {
        panic!("ExistingTypePath attribute only applies when the 'postgres' feature is enabled");
    }

    // we could allow a default value here but... I'm not very keen
    // let existing_mapping_path = existing_mapping_path
    //     .unwrap_or_else(|| format!("crate::schema::sql_types::{}", input.ident));

    let pg_internal_type = val_from_attrs(&input.attrs, "PgType");

    if existing_mapping_path.is_some() && pg_internal_type.is_some() {
        panic!("Cannot specify both `ExistingTypePath` and `PgType` attributes");
    }

    let pg_internal_type = pg_internal_type.unwrap_or(input.ident.to_string().to_snake_case());

    let new_diesel_mapping = val_from_attrs(&input.attrs, "DieselType");
    if existing_mapping_path.is_some() && new_diesel_mapping.is_some() {
        panic!("Cannot specify both `ExistingTypePath` and `DieselType` attributes");
    }
    let new_diesel_mapping =
        new_diesel_mapping.unwrap_or_else(|| format!("{}Mapping", input.ident));

    // Maintain backwards compatibility by defaulting to snake case.
    let case_style =
        val_from_attrs(&input.attrs, "DbValueStyle").unwrap_or_else(|| "snake_case".to_string());
    let case_style = CaseStyle::from_string(&case_style);

    let existing_mapping_path = existing_mapping_path.map(|v| {
        v.parse::<proc_macro2::TokenStream>()
            .expect("ExistingTypePath is not a valid token")
    });
    let new_diesel_mapping = Ident::new(new_diesel_mapping.as_ref(), Span::call_site());
    if let Data::Enum(syn::DataEnum {
        variants: data_variants,
        ..
    }) = input.data
    {
        generate_derive_enum_impls(
            &existing_mapping_path,
            &new_diesel_mapping,
            &pg_internal_type,
            case_style,
            &input.ident,
            &data_variants,
        )
    } else {
        syn::Error::new(
            Span::call_site(),
            "derive(DbEnum) can only be applied to enums",
        )
        .to_compile_error()
        .into()
    }
}

fn val_from_attrs(attrs: &[Attribute], attrname: &str) -> Option<String> {
    for attr in attrs {
        if attr.path.is_ident(attrname) {
            match attr.parse_meta().ok()? {
                Meta::NameValue(MetaNameValue {
                    lit: Lit::Str(lit_str),
                    ..
                }) => return Some(lit_str.value()),
                _ => panic!(
                    "Attribute '{}' must have form: {} = \"value\"",
                    attrname, attrname
                ),
            }
        }
    }
    None
}

/// Defines the casing for the database representation.  Follows serde naming convention.
#[derive(Copy, Clone, Debug, PartialEq)]
enum CaseStyle {
    Camel,
    Kebab,
    Pascal,
    Upper,
    ScreamingSnake,
    Snake,
    Verbatim,
}

impl CaseStyle {
    fn from_string(name: &str) -> Self {
        match name {
            "camelCase" => CaseStyle::Camel,
            "kebab-case" => CaseStyle::Kebab,
            "PascalCase" => CaseStyle::Pascal,
            "SCREAMING_SNAKE_CASE" => CaseStyle::ScreamingSnake,
            "UPPERCASE" => CaseStyle::Upper,
            "snake_case" => CaseStyle::Snake,
            "verbatim" | "verbatimcase" => CaseStyle::Verbatim,
            s => panic!("unsupported casing: `{}`", s),
        }
    }
}

fn generate_derive_enum_impls(
    existing_mapping_path: &Option<proc_macro2::TokenStream>,
    new_diesel_mapping: &Ident,
    pg_internal_type: &str,
    case_style: CaseStyle,
    enum_ty: &Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> TokenStream {
    let modname = Ident::new(&format!("db_enum_impl_{}", enum_ty), Span::call_site());
    let variant_ids: Vec<proc_macro2::TokenStream> = variants
        .iter()
        .map(|variant| {
            if let Fields::Unit = variant.fields {
                let id = &variant.ident;
                quote! {
                    #enum_ty::#id
                }
            } else {
                panic!("Variants must be fieldless")
            }
        })
        .collect();

    let variants_db: Vec<String> = variants
        .iter()
        .map(|variant| {
            val_from_attrs(&variant.attrs, "db_rename")
                .unwrap_or_else(|| stylize_value(&variant.ident.to_string(), case_style))
        })
        .collect();
    let variants_db_bytes: Vec<LitByteStr> = variants_db
        .iter()
        .map(|variant_str| LitByteStr::new(variant_str.as_bytes(), Span::call_site()))
        .collect();

    let common = generate_common(enum_ty, &variant_ids, &variants_db, &variants_db_bytes);
    let (diesel_mapping_def, diesel_mapping_use) =
        // Skip this part if we already have an existing mapping
        if existing_mapping_path.is_some() {
            (None, None)
        } else {
            let new_diesel_mapping_def = generate_new_diesel_mapping(new_diesel_mapping, pg_internal_type);
            let common_impls_on_new_diesel_mapping =
                generate_common_impls(&quote! { #new_diesel_mapping }, enum_ty);
            (
                Some(quote! {
                    #new_diesel_mapping_def
                    #common_impls_on_new_diesel_mapping
                }),
                Some(quote! {
                    pub use self::#modname::#new_diesel_mapping;
                }),
            )
        };

    let pg_impl = if cfg!(feature = "postgres") {
        match existing_mapping_path {
            Some(path) => {
                let common_impls_on_existing_diesel_mapping = generate_common_impls(path, enum_ty);
                let postgres_impl = generate_postgres_impl(path, enum_ty, true);
                Some(quote! {
                    #common_impls_on_existing_diesel_mapping
                    #postgres_impl
                })
            }
            None => Some(generate_postgres_impl(
                &quote! { #new_diesel_mapping },
                enum_ty,
                false,
            )),
        }
    } else {
        None
    };

    let mysql_impl = if cfg!(feature = "mysql") {
        Some(generate_mysql_impl(new_diesel_mapping, enum_ty))
    } else {
        None
    };

    let sqlite_impl = if cfg!(feature = "sqlite") {
        Some(generate_sqlite_impl(new_diesel_mapping, enum_ty))
    } else {
        None
    };

    let imports = quote! {
        use super::*;
        use diesel::{
            backend::{self, Backend},
            deserialize::{self, FromSql},
            expression::AsExpression,
            internal::derives::as_expression::Bound,
            query_builder::{bind_collector::RawBytesBindCollector, QueryId},
            row::Row,
            serialize::{self, IsNull, Output, ToSql},
            sql_types::*,
            Queryable,
        };
        use std::io::Write;
    };

    let quoted = quote! {
        #diesel_mapping_use
        #[allow(non_snake_case)]
        mod #modname {
            #imports

            #common
            #diesel_mapping_def
            #pg_impl
            #mysql_impl
            #sqlite_impl
        }
    };

    quoted.into()
}

fn stylize_value(value: &str, style: CaseStyle) -> String {
    match style {
        CaseStyle::Camel => value.to_lower_camel_case(),
        CaseStyle::Kebab => value.to_kebab_case(),
        CaseStyle::Pascal => value.to_upper_camel_case(),
        CaseStyle::Upper => value.to_uppercase(),
        CaseStyle::ScreamingSnake => value.to_shouty_snake_case(),
        CaseStyle::Snake => value.to_snake_case(),
        CaseStyle::Verbatim => value.to_string(),
    }
}

fn generate_common(
    enum_ty: &Ident,
    variants_rs: &[proc_macro2::TokenStream],
    variants_db: &[String],
    variants_db_bytes: &[LitByteStr],
) -> proc_macro2::TokenStream {
    quote! {
        fn db_str_representation(e: &#enum_ty) -> &'static str {
            match *e {
                #(#variants_rs => #variants_db,)*
            }
        }

        fn from_db_binary_representation(bytes: &[u8]) -> deserialize::Result<#enum_ty> {
            match bytes {
                #(#variants_db_bytes => Ok(#variants_rs),)*
                v => Err(format!("Unrecognized enum variant: '{}'",
                    String::from_utf8_lossy(v)).into()),
            }
        }
    }
}

fn generate_new_diesel_mapping(
    new_diesel_mapping: &Ident,
    pg_internal_type: &str,
) -> proc_macro2::TokenStream {
    // Note - we only generate a new mapping for mysql and sqlite, postgres
    // should already have one
    quote! {
        #[derive(SqlType, Clone)]
        #[diesel(mysql_type(name = "Enum"))]
        #[diesel(sqlite_type(name = "Text"))]
        #[diesel(postgres_type(name = #pg_internal_type))]
        pub struct #new_diesel_mapping;
    }
}

fn generate_common_impls(
    diesel_mapping: &proc_macro2::TokenStream,
    enum_ty: &Ident,
) -> proc_macro2::TokenStream {
    quote! {

        // NOTE: at some point this impl will no longer be necessary
        // for diesel-cli schemas
        // See https://github.com/adwhit/diesel-derive-enum/issues/10
        // and https://github.com/adwhit/diesel-derive-enum/pull/79
        impl QueryId for #diesel_mapping {
            type QueryId = #diesel_mapping;
            const HAS_STATIC_QUERY_ID: bool = true;
        }

        impl AsExpression<#diesel_mapping> for #enum_ty {
            type Expression = Bound<#diesel_mapping, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl AsExpression<Nullable<#diesel_mapping>> for #enum_ty {
            type Expression = Bound<Nullable<#diesel_mapping>, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl<'a> AsExpression<#diesel_mapping> for &'a #enum_ty {
            type Expression = Bound<#diesel_mapping, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl<'a> AsExpression<Nullable<#diesel_mapping>> for &'a #enum_ty {
            type Expression = Bound<Nullable<#diesel_mapping>, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl<'a, 'b> AsExpression<#diesel_mapping> for &'a &'b #enum_ty {
            type Expression = Bound<#diesel_mapping, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl<'a, 'b> AsExpression<Nullable<#diesel_mapping>> for &'a &'b #enum_ty {
            type Expression = Bound<Nullable<#diesel_mapping>, Self>;

            fn as_expression(self) -> Self::Expression {
                Bound::new(self)
            }
        }

        impl<DB> ToSql<Nullable<#diesel_mapping>, DB> for #enum_ty
        where
            DB: Backend,
            Self: ToSql<#diesel_mapping, DB>,
        {
            fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
                ToSql::<#diesel_mapping, DB>::to_sql(self, out)
            }
        }
    }
}

fn generate_postgres_impl(
    diesel_mapping: &proc_macro2::TokenStream,
    enum_ty: &Ident,
    with_clone: bool,
) -> proc_macro2::TokenStream {
    // If the type was generated by postgres, we have to manually add a clone impl,
    // if generated by 'us' it has already been done
    let clone_impl = if with_clone {
        Some(quote! {
            impl Clone for #diesel_mapping {
                fn clone(&self) -> Self {
                    #diesel_mapping
                }
            }
        })
    } else {
        None
    };

    quote! {
        mod pg_impl {
            use super::*;
            use diesel::pg::{Pg, PgValue};

            // #clone_impl

            impl FromSql<#diesel_mapping, Pg> for #enum_ty {
                fn from_sql(raw: PgValue) -> deserialize::Result<Self> {
                    from_db_binary_representation(raw.as_bytes())
                }
            }

            impl ToSql<#diesel_mapping, Pg> for #enum_ty
            {
                fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> serialize::Result {
                    out.write_all(db_str_representation(self).as_bytes())?;
                    Ok(IsNull::No)
                }
            }

            impl Queryable<#diesel_mapping, Pg> for #enum_ty {
                type Row = Self;

                fn build(row: Self::Row) -> deserialize::Result<Self> {
                    Ok(row)
                }
            }
        }
    }
}

fn generate_mysql_impl(diesel_mapping: &Ident, enum_ty: &Ident) -> proc_macro2::TokenStream {
    quote! {
        mod mysql_impl {
            use super::*;
            use diesel;
            use diesel::mysql::{Mysql, MysqlValue};

            impl FromSql<#diesel_mapping, Mysql> for #enum_ty {
                fn from_sql(raw: MysqlValue) -> deserialize::Result<Self> {
                    from_db_binary_representation(raw.as_bytes())
                }
            }

            impl ToSql<#diesel_mapping, Mysql> for #enum_ty
            {
                fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Mysql>) -> serialize::Result {
                    out.write_all(db_str_representation(self).as_bytes())?;
                    Ok(IsNull::No)
                }
            }

            impl Queryable<#diesel_mapping, Mysql> for #enum_ty {
                type Row = Self;

                fn build(row: Self::Row) -> deserialize::Result<Self> {
                    Ok(row)
                }
            }
        }
    }
}

fn generate_sqlite_impl(diesel_mapping: &Ident, enum_ty: &Ident) -> proc_macro2::TokenStream {
    quote! {
        mod sqlite_impl {
            use super::*;
            use diesel;
            use diesel::sql_types;
            use diesel::sqlite::Sqlite;

            impl FromSql<#diesel_mapping, Sqlite> for #enum_ty {
                fn from_sql(value: backend::RawValue<Sqlite>) -> deserialize::Result<Self> {
                    let bytes = <Vec<u8> as FromSql<sql_types::Binary, Sqlite>>::from_sql(value)?;
                    from_db_binary_representation(bytes.as_slice())
                }
            }

            impl ToSql<#diesel_mapping, Sqlite> for #enum_ty {
                fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
                    <str as ToSql<sql_types::Text, Sqlite>>::to_sql(db_str_representation(self), out)
                }
            }

            impl Queryable<#diesel_mapping, Sqlite> for #enum_ty {
                type Row = Self;

                fn build(row: Self::Row) -> deserialize::Result<Self> {
                    Ok(row)
                }
            }
        }
    }
}
