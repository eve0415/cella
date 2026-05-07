use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{ItemFn, LitStr, Token, punctuated::Punctuated};

const KNOWN_RUNTIMES: &[&str] = &[
    "docker",
    "compose",
    "buildx",
    "podman",
    "apple_container",
    "orbstack",
    "colima",
    "lima",
    "network",
    "container_runtime",
];

struct RuntimeRequirement {
    negated: bool,
    name: String,
}

struct RuntimeTestArgs {
    requirements: Vec<RuntimeRequirement>,
    flavor: Option<String>,
}

impl Parse for RuntimeTestArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut requirements = Vec::new();
        let mut flavor = None;

        if input.is_empty() {
            return Ok(Self {
                requirements,
                flavor,
            });
        }

        let items = Punctuated::<RuntimeArg, Token![,]>::parse_terminated(input)?;
        for item in items {
            match item {
                RuntimeArg::Runtime(req) => requirements.push(req),
                RuntimeArg::Flavor(f) => flavor = Some(f),
            }
        }

        Ok(Self {
            requirements,
            flavor,
        })
    }
}

enum RuntimeArg {
    Runtime(RuntimeRequirement),
    Flavor(String),
}

impl Parse for RuntimeArg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(syn::Ident) {
            let ident: syn::Ident = input.parse()?;
            if ident == "flavor" {
                let _: Token![=] = input.parse()?;
                let value: LitStr = input.parse()?;
                return Ok(Self::Flavor(value.value()));
            }
            if !KNOWN_RUNTIMES.contains(&ident.to_string().as_str()) {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "unknown runtime `{ident}`. Expected one of: {}",
                        KNOWN_RUNTIMES.join(", ")
                    ),
                ));
            }
            Ok(Self::Runtime(RuntimeRequirement {
                negated: false,
                name: ident.to_string(),
            }))
        } else if input.peek(Token![!]) {
            let _: Token![!] = input.parse()?;
            let ident: syn::Ident = input.parse()?;
            if !KNOWN_RUNTIMES.contains(&ident.to_string().as_str()) {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "unknown runtime `{ident}`. Expected one of: {}",
                        KNOWN_RUNTIMES.join(", ")
                    ),
                ));
            }
            Ok(Self::Runtime(RuntimeRequirement {
                negated: true,
                name: ident.to_string(),
            }))
        } else {
            Err(input.error("expected runtime name or `!runtime_name`"))
        }
    }
}

#[proc_macro_attribute]
pub fn runtime_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as RuntimeTestArgs);
    let input = syn::parse_macro_input!(item as ItemFn);

    match expand_runtime_test(&args, &input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_runtime_test(
    args: &RuntimeTestArgs,
    input: &ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    let is_async = input.sig.asyncness.is_some();
    let fn_name = &input.sig.ident;
    let fn_name_str = fn_name.to_string();
    let vis = &input.vis;
    let attrs = &input.attrs;
    let sig = &input.sig;
    let body = &input.block;

    let preamble = build_preamble(args, &fn_name_str, is_async)?;

    let test_attr = if is_async {
        args.flavor.as_ref().map_or_else(
            || quote! { #[tokio::test] },
            |flavor| quote! { #[tokio::test(flavor = #flavor)] },
        )
    } else {
        if args.flavor.is_some() {
            return Err(syn::Error::new_spanned(
                fn_name,
                "flavor can only be used with async test functions",
            ));
        }
        quote! { #[test] }
    };

    Ok(quote! {
        #(#attrs)*
        #test_attr
        #vis #sig {
            #preamble
            #body
        }
    })
}

fn build_preamble(
    args: &RuntimeTestArgs,
    fn_name: &str,
    is_async: bool,
) -> syn::Result<proc_macro2::TokenStream> {
    if args.requirements.is_empty() {
        let check = if is_async {
            quote! { cella_testing::detect::container_runtime_available().await }
        } else {
            quote! { cella_testing::detect::container_runtime_available_sync() }
        };
        return Ok(quote! {
            if !#check {
                println!("skipping {}: no container runtime available", #fn_name);
                return;
            }
        });
    }

    let has_negated = args.requirements.iter().any(|r| r.negated);
    let has_positive = args.requirements.iter().any(|r| !r.negated);

    if has_negated && has_positive {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "cannot mix negated (!) and positive runtime requirements",
        ));
    }

    if has_negated {
        let excluded: Vec<&str> = args.requirements.iter().map(|r| r.name.as_str()).collect();
        let check = if is_async {
            quote! { cella_testing::detect::container_runtime_available_except(&[#(#excluded),*]).await }
        } else {
            quote! { cella_testing::detect::container_runtime_available_except_sync(&[#(#excluded),*]) }
        };
        return Ok(quote! {
            if !#check {
                println!("skipping {}: no container runtime available (excluding {})", #fn_name, [#(#excluded),*].join(", "));
                return;
            }
        });
    }

    let mut checks = proc_macro2::TokenStream::new();
    for req in &args.requirements {
        let fn_async = format_ident!("{}_available", req.name);
        let fn_sync = format_ident!("{}_available_sync", req.name);
        let name = &req.name;
        let check = if is_async {
            quote! {
                if !cella_testing::detect::#fn_async().await {
                    println!("skipping {}: {} not available", #fn_name, #name);
                    return;
                }
            }
        } else {
            quote! {
                if !cella_testing::detect::#fn_sync() {
                    println!("skipping {}: {} not available", #fn_name, #name);
                    return;
                }
            }
        };
        checks.extend(check);
    }

    Ok(checks)
}
