use quote::ToTokens;
use syn::{ExprPath, meta::ParseNestedMeta};

use super::EMBED;

const EMBED_WITH: &str = "embed_with";

/// Finds and returns fields with #[embed(embed_with = "...")] attribute tags only.
/// Also returns the "..." part of the tag (ie. the custom function).
pub(crate) fn custom_embed_fields(
    data_struct: &syn::DataStruct,
) -> syn::Result<Vec<(&syn::Field, syn::ExprPath)>> {
    data_struct
        .fields
        .iter()
        .filter_map(|field| {
            field
                .attrs
                .iter()
                .filter_map(|attribute| match attribute.is_custom() {
                    Ok(true) => match attribute.expand_tag() {
                        Ok(path) => Some(Ok((field, path))),
                        Err(e) => Some(Err(e)),
                    },
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                })
                .next()
        })
        .collect::<Result<Vec<_>, _>>()
}

trait CustomAttributeParser {
    // Determine if field is tagged with an #[embed(embed_with = "...")] attribute.
    fn is_custom(&self) -> syn::Result<bool>;

    // Get the "..." part of the #[embed(embed_with = "...")] attribute.
    // Ex: If attribute is tagged with #[embed(embed_with = "my_embed")], returns "my_embed".
    fn expand_tag(&self) -> syn::Result<syn::ExprPath>;
}

impl CustomAttributeParser for syn::Attribute {
    fn is_custom(&self) -> syn::Result<bool> {
        // Check that the attribute is a list.
        match &self.meta {
            syn::Meta::List(meta) => {
                if meta.tokens.is_empty() {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        };

        // Check the first attribute tag (the first "embed")
        if !self.path().is_ident(EMBED) {
            return Ok(false);
        }

        self.parse_nested_meta(|meta| {
            // Parse the meta attribute as an expression. Need this to compile.
            meta.value()?.parse::<syn::Expr>()?;

            if meta.path.is_ident(EMBED_WITH) {
                Ok(())
            } else {
                let path = meta.path.to_token_stream().to_string().replace(' ', "");
                Err(syn::Error::new_spanned(
                    meta.path,
                    format_args!("unknown embedding field attribute `{path}`"),
                ))
            }
        })?;

        Ok(true)
    }

    fn expand_tag(&self) -> syn::Result<syn::ExprPath> {
        fn function_path(meta: &ParseNestedMeta<'_>) -> syn::Result<ExprPath> {
            // #[embed(embed_with = "...")]
            let expr = meta.value()?.parse::<syn::Expr>()?;
            let mut value = &expr;
            while let syn::Expr::Group(e) = value {
                value = &e.expr;
            }
            let string = if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(lit_str),
                ..
            }) = value
            {
                let suffix = lit_str.suffix();
                if !suffix.is_empty() {
                    return Err(syn::Error::new_spanned(
                        lit_str,
                        format!("unexpected suffix `{suffix}` on string literal"),
                    ));
                }
                lit_str.clone()
            } else {
                return Err(syn::Error::new_spanned(
                    value,
                    format!(
                        "expected {EMBED_WITH} attribute to be a string: `{EMBED_WITH} = \"...\"`"
                    ),
                ));
            };

            string.parse()
        }

        let mut custom_func_path = None;

        self.parse_nested_meta(|meta| match function_path(&meta) {
            Ok(path) => {
                custom_func_path = Some(path);
                Ok(())
            }
            Err(e) => Err(e),
        })?;

        custom_func_path.ok_or_else(|| {
            syn::Error::new_spanned(
                self,
                format!("expected {EMBED_WITH} attribute: `{EMBED_WITH} = \"...\"`"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a struct and collect its custom embed fields.
    fn custom_fields(source: &str) -> syn::Result<Vec<String>> {
        let input = syn::parse_str::<syn::DeriveInput>(source).expect("test input parses");
        let syn::Data::Struct(data_struct) = input.data else {
            panic!("test input must be a struct");
        };
        Ok(custom_embed_fields(&data_struct)?
            .into_iter()
            .map(|(_, path)| path.path.segments.last().unwrap().ident.to_string())
            .collect())
    }

    #[test]
    fn tagged_fields_report_their_function_paths() {
        let paths = custom_fields(r#"struct S { #[embed(embed_with = "my_embed")] a: String }"#)
            .expect("valid attribute");
        assert_eq!(paths, vec!["my_embed".to_string()]);
    }

    #[test]
    fn group_wrapped_string_literals_resolve_to_the_path() {
        // Invisible `Delimiter::None` groups (how macro-passed expressions
        // reach the derive) cannot be typed in source text, so build one.
        let mut input =
            syn::parse_str::<syn::DeriveInput>(r#"struct S { #[embed(embed_with = "my_embed")] a: String }"#)
                .expect("test input parses");
        let syn::Data::Struct(ref mut data_struct) = input.data else {
            panic!("test input must be a struct");
        };
        let field = data_struct
            .fields
            .iter_mut()
            .next()
            .expect("one field");
        for attribute in &mut field.attrs {
            if !attribute.path().is_ident(EMBED) {
                continue;
            }
            if let syn::Meta::List(list) = &mut attribute.meta {
                let grouped = proc_macro2::Group::new(
                    proc_macro2::Delimiter::None,
                    quote::quote!("my_embed"),
                );
                list.tokens = quote::quote!(embed_with = #grouped);
            }
        }

        let paths = custom_embed_fields(data_struct)
            .expect("group-wrapped string literal is accepted")
            .into_iter()
            .map(|(_, path)| path.path.segments.last().unwrap().ident.to_string())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["my_embed".to_string()]);
    }

    #[test]
    fn empty_and_non_list_and_foreign_attributes_are_not_custom() {
        // `#[embed()]` (empty list) and `#[embed]`/`#[embed = "..."]`
        // (non-list metas) are filtered out before tag expansion, and
        // attributes under a different path are ignored entirely.
        let paths = custom_fields(
            r#"struct S {
                #[embed()]
                #[embed]
                #[embed = "plain"]
                #[serde(skip)]
                a: String,
            }"#,
        )
        .expect("none of these are custom attributes");
        assert!(paths.is_empty());
    }

    #[test]
    fn unknown_attribute_keys_are_rejected() {
        let error = custom_fields(r#"struct S { #[embed(bogus = "x")] a: String }"#)
            .expect_err("unknown attribute key must fail");
        assert!(
            error.to_string().contains("unknown embedding field attribute `bogus`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn non_string_values_are_rejected() {
        let error = custom_fields(r#"struct S { #[embed(embed_with = 42)] a: String }"#)
            .expect_err("a non-string value must fail");
        assert!(
            error.to_string().contains("expected embed_with attribute to be a string"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn string_literal_suffixes_are_rejected() {
        let error =
            custom_fields(r#"struct S { #[embed(embed_with = "my_embed"sfx)] a: String }"#)
                .expect_err("a suffixed string literal must fail");
        assert!(
            error.to_string().contains("unexpected suffix `sfx`"),
            "unexpected error: {error}"
        );
    }
}
