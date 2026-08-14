//! Classification of function parameters as the runtime execution context.

use syn::{Attribute, Ident, Meta, Type};

use crate::resolve::CrateRefs;

/// Returns whether `ty` uses an unambiguous fully qualified path to Rig's
/// tool execution context.
///
/// Procedural macros cannot resolve imported type names. Matching only the
/// last `ToolContext` path segment would therefore steal unrelated application
/// types with the same name, so only paths rooted at a crate name Rig resolves
/// to in this build (including Cargo renames) are recognized. Imported aliases
/// use the explicit `#[rig(context)]` parameter marker instead.
fn is_tool_context_type(ty: &Type, refs: &CrateRefs) -> bool {
    let ty = match ty {
        Type::Group(group) => &*group.elem,
        Type::Paren(paren) => &*paren.elem,
        ty => ty,
    };

    let Type::Path(type_path) = ty else {
        return false;
    };
    let segments = type_path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();

    refs.is_context_path(&segments)
}

/// Whether a function parameter explicitly marks itself as Rig's runtime
/// context. The marker is removed from the emitted function.
pub(crate) fn has_tool_context_marker(attrs: &[Attribute]) -> syn::Result<bool> {
    let mut marked = false;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("rig")) {
        if marked {
            return Err(syn::Error::new_spanned(
                attr,
                "duplicate `#[rig(context)]` parameter marker",
            ));
        }

        let Meta::List(list) = &attr.meta else {
            return Err(syn::Error::new_spanned(
                attr,
                "expected `#[rig(context)]` on the runtime context parameter",
            ));
        };
        let marker: Ident = list.parse_args().map_err(|_| {
            syn::Error::new_spanned(
                attr,
                "expected `#[rig(context)]` on the runtime context parameter",
            )
        })?;
        if marker != "context" {
            return Err(syn::Error::new_spanned(
                marker,
                "the only supported parameter marker is `#[rig(context)]`",
            ));
        }
        marked = true;
    }
    Ok(marked)
}

/// Classify a function parameter as the distinguished execution context.
///
/// An owned or shared `ToolContext` is almost certainly an authoring mistake:
/// tools need the exact mutable context supplied by the runtime so result
/// metadata and mutations remain visible to the caller.
pub(crate) fn is_tool_context_parameter(
    ty: &Type,
    explicitly_marked: bool,
    refs: &CrateRefs,
) -> syn::Result<bool> {
    let ty = match ty {
        Type::Group(group) => &*group.elem,
        Type::Paren(paren) => &*paren.elem,
        ty => ty,
    };

    if let Type::Reference(reference) = ty
        && (explicitly_marked || is_tool_context_type(&reference.elem, refs))
    {
        if reference.mutability.is_none() {
            return Err(syn::Error::new_spanned(
                ty,
                "a `ToolContext` parameter must have type `&mut ToolContext`",
            ));
        }

        return Ok(true);
    }

    if explicitly_marked || is_tool_context_type(ty, refs) {
        return Err(syn::Error::new_spanned(
            ty,
            "a `ToolContext` parameter must have type `&mut ToolContext`",
        ));
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_param_attrs(source: &str) -> Vec<Attribute> {
        let function = syn::parse_str::<syn::ItemFn>(source).expect("test function parses");
        match function.sig.inputs.first() {
            Some(syn::FnArg::Typed(pat_type)) => pat_type.attrs.clone(),
            _ => panic!("test function needs a typed first parameter"),
        }
    }

    fn marker(source: &str) -> syn::Result<bool> {
        has_tool_context_marker(&first_param_attrs(source))
    }

    #[test]
    fn duplicate_context_markers_are_rejected() {
        let error = marker("fn f(#[rig(context)] #[rig(context)] a: &mut T) {}")
            .err()
            .expect("duplicate marker rejected");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn non_list_rig_attributes_are_rejected() {
        let error = marker(r#"fn f(#[rig = "context"] a: &mut T) {}"#)
            .err()
            .expect("name-value marker rejected");
        assert!(error.to_string().contains("expected `#[rig(context)]`"));
    }

    #[test]
    fn multi_token_markers_are_rejected() {
        let error = marker("fn f(#[rig(context, extra)] a: &mut T) {}")
            .err()
            .expect("multi-token marker rejected");
        assert!(error.to_string().contains("expected `#[rig(context)]`"));
    }

    #[test]
    fn only_the_context_marker_is_supported() {
        let error = marker("fn f(#[rig(bogus)] a: &mut T) {}")
            .err()
            .expect("unknown marker rejected");
        assert!(error.to_string().contains("only supported parameter marker"));
    }

    #[test]
    fn plain_parameters_carry_no_marker() {
        assert_eq!(marker("fn f(a: i32) {}").expect("unmarked parses"), false);
    }

    fn wrapped(inner: &str, group: bool) -> Type {
        let inner: Type = syn::parse_str(inner).expect("inner type parses");
        if group {
            Type::Group(syn::TypeGroup {
                group_token: Default::default(),
                elem: Box::new(inner),
            })
        } else {
            Type::Paren(syn::TypeParen {
                paren_token: Default::default(),
                elem: Box::new(inner),
            })
        }
    }

    #[test]
    fn non_path_types_are_never_context() {
        let refs = CrateRefs::resolve();
        let tuple: Type = syn::parse_str("(i32,)").expect("tuple parses");
        assert!(!is_tool_context_parameter(&tuple, false, &refs).expect("tuple classified"));
        assert!(!is_tool_context_type(&tuple, &refs));
    }

    #[test]
    fn group_and_paren_wrappers_are_transparent() {
        let refs = CrateRefs::resolve();

        // A transparent wrapper around a non-path type stays a non-context.
        let wrapped_tuple = wrapped("(i32,)", true);
        assert!(
            !is_tool_context_parameter(&wrapped_tuple, false, &refs).expect("group classified")
        );

        // The path classifier itself also unwraps transparent wrappers.
        let wrapped_path = wrapped("some::ToolContext", true);
        is_tool_context_type(&wrapped_path, &refs);

        // A transparent wrapper around an explicitly marked shared reference
        // is reported as a context (with the mutability checked separately).
        let marked = wrapped("&mut Context", false);
        assert!(
            is_tool_context_parameter(&marked, true, &refs).expect("paren classified")
        );
    }
}
