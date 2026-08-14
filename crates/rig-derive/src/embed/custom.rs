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
                .filter_map(|attribute| match attribute.custom_embed_path() {
                    Ok(Some(path)) => Some(Ok((field, path))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                })
                .next()
        })
        .collect::<Result<Vec<_>, _>>()
}

trait CustomAttributeParser {
    // Parse `#[embed(embed_with = "...")]` in a single pass: `Ok(Some(path))`
    // when the attribute is a well-formed custom-embed tag (the "..." part of
    // the tag, ie. the custom function), `Ok(None)` when the attribute is not
    // an `#[embed(...)]` list (or is an empty `#[embed()]`), and `Err` on a
    // malformed tag.
    fn custom_embed_path(&self) -> syn::Result<Option<syn::ExprPath>>;
}

impl CustomAttributeParser for syn::Attribute {
    fn custom_embed_path(&self) -> syn::Result<Option<syn::ExprPath>> {
        // Only `#[embed(...)]` lists can be custom tags; an empty `#[embed()]`
        // is not one either. Rejecting empty lists up front is what guarantees
        // the fold below always sees at least one nested item: syn's
        // `parse_nested_meta` invokes its callback before any successful exit,
        // and its zero-item fast path requires an empty token stream, which
        // the non-empty check above rules out. So on `Ok`, the path below is
        // always `Some`; the `Ok(None)` return only ever means "not custom".
        let syn::Meta::List(meta) = &self.meta else {
            return Ok(None);
        };
        if !self.path().is_ident(EMBED) {
            return Ok(None);
        }
        if meta.tokens.is_empty() {
            return Ok(None);
        }

        // Every nested item must be `embed_with = "..."`; when one is
        // repeated, the last one wins.
        let mut custom_func_path = None;
        self.parse_nested_meta(|meta| {
            if !meta.path.is_ident(EMBED_WITH) {
                let path = meta.path.to_token_stream().to_string().replace(' ', "");
                return Err(syn::Error::new_spanned(
                    &meta.path,
                    format!("unknown embedding field attribute `{path}`"),
                ));
            }
            custom_func_path = Some(function_path(&meta)?);
            Ok(())
        })?;

        Ok(custom_func_path)
    }
}

// Get the "..." part of the #[embed(embed_with = "...")] attribute.
// Ex: If attribute is tagged with #[embed(embed_with = "my_embed")], returns "my_embed".
fn function_path(meta: &ParseNestedMeta<'_>) -> syn::Result<ExprPath> {
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
            format!("expected {EMBED_WITH} attribute to be a string: `{EMBED_WITH} = \"...\"`"),
        ));
    };

    string.parse()
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

    #[test]
    fn an_empty_invisible_group_in_the_list_is_rejected_not_skipped() {
        // A proc macro can emit an attribute whose list content is a single
        // empty `Delimiter::None` group — not expressible in source text. The
        // non-empty check sees the group, so parsing must surface it as an
        // error rather than silently reporting the field as not custom.
        let mut input = syn::parse_str::<syn::DeriveInput>(
            r#"struct S { #[embed()] a: String }"#,
        )
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
            if let syn::Meta::List(list) = &mut attribute.meta {
                let empty = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote::quote!());
                list.tokens = quote::quote!(#empty);
            }
        }

        let error = custom_embed_fields(data_struct)
            .map(|fields| fields.len())
            .expect_err("an empty invisible group must fail, not skip");
        assert!(
            !error.to_string().is_empty(),
            "unexpected error: {error}"
        );
    }
}
