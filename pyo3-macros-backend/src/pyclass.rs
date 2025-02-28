// Copyright (c) 2017-present PyO3 Project and Contributors

use crate::method::{FnType, SelfType};
use crate::pyimpl::PyClassMethodsType;
use crate::pymethod::{impl_py_getter_def, impl_py_setter_def, PropertyType};
use crate::utils;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parse_quote, spanned::Spanned, Expr, Token};

/// The parsed arguments of the pyclass macro
pub struct PyClassArgs {
    pub freelist: Option<syn::Expr>,
    pub name: Option<syn::Ident>,
    pub base: syn::TypePath,
    pub has_dict: bool,
    pub has_weaklist: bool,
    pub is_gc: bool,
    pub is_basetype: bool,
    pub has_extends: bool,
    pub has_unsendable: bool,
    pub module: Option<syn::LitStr>,
}

impl Parse for PyClassArgs {
    fn parse(input: ParseStream) -> syn::parse::Result<Self> {
        let mut slf = PyClassArgs::default();

        let vars = Punctuated::<Expr, Token![,]>::parse_terminated(input)?;
        for expr in vars {
            slf.add_expr(&expr)?;
        }
        Ok(slf)
    }
}

impl Default for PyClassArgs {
    fn default() -> Self {
        PyClassArgs {
            freelist: None,
            name: None,
            module: None,
            base: parse_quote! { pyo3::PyAny },
            has_dict: false,
            has_weaklist: false,
            is_gc: false,
            is_basetype: false,
            has_extends: false,
            has_unsendable: false,
        }
    }
}

impl PyClassArgs {
    /// Adda single expression from the comma separated list in the attribute, which is
    /// either a single word or an assignment expression
    fn add_expr(&mut self, expr: &Expr) -> syn::parse::Result<()> {
        match expr {
            syn::Expr::Path(exp) if exp.path.segments.len() == 1 => self.add_path(exp),
            syn::Expr::Assign(assign) => self.add_assign(assign),
            _ => bail_spanned!(expr.span() => "failed to parse arguments"),
        }
    }

    /// Match a key/value flag
    fn add_assign(&mut self, assign: &syn::ExprAssign) -> syn::Result<()> {
        let syn::ExprAssign { left, right, .. } = assign;
        let key = match &**left {
            syn::Expr::Path(exp) if exp.path.segments.len() == 1 => {
                exp.path.segments.first().unwrap().ident.to_string()
            }
            _ => bail_spanned!(assign.span() => "failed to parse arguments"),
        };

        macro_rules! expected {
            ($expected: literal) => {
                expected!($expected, right.span())
            };
            ($expected: literal, $span: expr) => {
                bail_spanned!($span => concat!("expected ", $expected));
            };
        }

        match key.as_str() {
            "freelist" => {
                // We allow arbitrary expressions here so you can e.g. use `8*64`
                self.freelist = Some(syn::Expr::clone(right));
            }
            "name" => match &**right {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(lit),
                    ..
                }) => {
                    self.name = Some(lit.parse().map_err(|_| {
                        err_spanned!(
                                lit.span() => "expected a single identifier in double-quotes")
                    })?);
                }
                syn::Expr::Path(exp) if exp.path.segments.len() == 1 => {
                    bail_spanned!(
                        exp.span() => format!(
                            "since PyO3 0.13 a pyclass name should be in double-quotes, \
                            e.g. \"{}\"",
                            exp.path.get_ident().expect("path has 1 segment")
                        )
                    );
                }
                _ => expected!("type name (e.g. \"Name\")"),
            },
            "extends" => match &**right {
                syn::Expr::Path(exp) => {
                    self.base = syn::TypePath {
                        path: exp.path.clone(),
                        qself: None,
                    };
                    self.has_extends = true;
                }
                _ => expected!("type path (e.g., my_mod::BaseClass)"),
            },
            "module" => match &**right {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(lit),
                    ..
                }) => {
                    self.module = Some(lit.clone());
                }
                _ => expected!(r#"string literal (e.g., "my_mod")"#),
            },
            _ => expected!("one of freelist/name/extends/module", left.span()),
        };

        Ok(())
    }

    /// Match a single flag
    fn add_path(&mut self, exp: &syn::ExprPath) -> syn::Result<()> {
        let flag = exp.path.segments.first().unwrap().ident.to_string();
        match flag.as_str() {
            "gc" => {
                self.is_gc = true;
            }
            "weakref" => {
                self.has_weaklist = true;
            }
            "subclass" => {
                self.is_basetype = true;
            }
            "dict" => {
                self.has_dict = true;
            }
            "unsendable" => {
                self.has_unsendable = true;
            }
            _ => bail_spanned!(
                exp.path.span() => "expected one of gc/weakref/subclass/dict/unsendable"
            ),
        };
        Ok(())
    }
}

pub fn build_py_class(
    class: &mut syn::ItemStruct,
    attr: &PyClassArgs,
    methods_type: PyClassMethodsType,
) -> syn::Result<TokenStream> {
    let text_signature = utils::parse_text_signature_attrs(
        &mut class.attrs,
        &get_class_python_name(&class.ident, attr),
    )?;
    let doc = utils::get_doc(&class.attrs, text_signature, true)?;
    let mut descriptors = Vec::new();

    ensure_spanned!(
        class.generics.params.is_empty(),
        class.generics.span() => "#[pyclass] cannot have generic parameters"
    );

    match &mut class.fields {
        syn::Fields::Named(fields) => {
            for field in fields.named.iter_mut() {
                let field_descs = parse_descriptors(field)?;
                if !field_descs.is_empty() {
                    descriptors.push((field.clone(), field_descs));
                }
            }
        }
        syn::Fields::Unnamed(fields) => {
            for field in fields.unnamed.iter_mut() {
                let field_descs = parse_descriptors(field)?;
                if !field_descs.is_empty() {
                    descriptors.push((field.clone(), field_descs));
                }
            }
        }
        syn::Fields::Unit => { /* No fields for unit struct */ }
    }

    impl_class(&class.ident, &attr, doc, descriptors, methods_type)
}

/// Parses `#[pyo3(get, set)]`
fn parse_descriptors(item: &mut syn::Field) -> syn::Result<Vec<FnType>> {
    let mut descs = Vec::new();
    let mut new_attrs = Vec::new();
    for attr in item.attrs.drain(..) {
        if let Ok(syn::Meta::List(list)) = attr.parse_meta() {
            if list.path.is_ident("pyo3") {
                for meta in list.nested.iter() {
                    if let syn::NestedMeta::Meta(metaitem) = meta {
                        if metaitem.path().is_ident("get") {
                            descs.push(FnType::Getter(SelfType::Receiver { mutable: false }));
                        } else if metaitem.path().is_ident("set") {
                            descs.push(FnType::Setter(SelfType::Receiver { mutable: true }));
                        } else {
                            bail_spanned!(metaitem.span() => "only get and set are supported");
                        }
                    }
                }
            } else {
                new_attrs.push(attr)
            }
        } else {
            new_attrs.push(attr);
        }
    }
    item.attrs = new_attrs;
    Ok(descs)
}

/// To allow multiple #[pymethods] block, we define inventory types.
fn impl_methods_inventory(cls: &syn::Ident) -> TokenStream {
    // Try to build a unique type for better error messages
    let name = format!("Pyo3MethodsInventoryFor{}", cls.unraw());
    let inventory_cls = syn::Ident::new(&name, Span::call_site());

    quote! {
        #[doc(hidden)]
        pub struct #inventory_cls {
            methods: Vec<pyo3::class::PyMethodDefType>,
        }
        impl pyo3::class::impl_::PyMethodsInventory for #inventory_cls {
            fn new(methods: Vec<pyo3::class::PyMethodDefType>) -> Self {
                Self { methods }
            }
            fn get(&'static self) -> &'static [pyo3::class::PyMethodDefType] {
                &self.methods
            }
        }

        impl pyo3::class::impl_::HasMethodsInventory for #cls {
            type Methods = #inventory_cls;
        }

        pyo3::inventory::collect!(#inventory_cls);
    }
}

fn get_class_python_name<'a>(cls: &'a syn::Ident, attr: &'a PyClassArgs) -> &'a syn::Ident {
    attr.name.as_ref().unwrap_or(cls)
}

fn impl_class(
    cls: &syn::Ident,
    attr: &PyClassArgs,
    doc: syn::LitStr,
    descriptors: Vec<(syn::Field, Vec<FnType>)>,
    methods_type: PyClassMethodsType,
) -> syn::Result<TokenStream> {
    let cls_name = get_class_python_name(cls, attr).to_string();

    let extra = {
        if let Some(freelist) = &attr.freelist {
            quote! {
                impl pyo3::freelist::PyClassWithFreeList for #cls {
                    #[inline]
                    fn get_free_list(_py: pyo3::Python) -> &mut pyo3::freelist::FreeList<*mut pyo3::ffi::PyObject> {
                        static mut FREELIST: *mut pyo3::freelist::FreeList<*mut pyo3::ffi::PyObject> = 0 as *mut _;
                        unsafe {
                            if FREELIST.is_null() {
                                FREELIST = Box::into_raw(Box::new(
                                    pyo3::freelist::FreeList::with_capacity(#freelist)));
                            }
                            &mut *FREELIST
                        }
                    }
                }
            }
        } else {
            quote! {
                impl pyo3::pyclass::PyClassAlloc for #cls {}
            }
        }
    };

    let extra = if !descriptors.is_empty() {
        let path = syn::Path::from(syn::PathSegment::from(cls.clone()));
        let ty = syn::Type::from(syn::TypePath { path, qself: None });
        let desc_impls = impl_descriptors(&ty, descriptors)?;
        quote! {
            #desc_impls
            #extra
        }
    } else {
        extra
    };

    // insert space for weak ref
    let weakref = if attr.has_weaklist {
        quote! { pyo3::pyclass_slots::PyClassWeakRefSlot }
    } else if attr.has_extends {
        quote! { <Self::BaseType as pyo3::class::impl_::PyClassBaseType>::WeakRef }
    } else {
        quote! { pyo3::pyclass_slots::PyClassDummySlot }
    };
    let dict = if attr.has_dict {
        quote! { pyo3::pyclass_slots::PyClassDictSlot }
    } else if attr.has_extends {
        quote! { <Self::BaseType as pyo3::class::impl_::PyClassBaseType>::Dict }
    } else {
        quote! { pyo3::pyclass_slots::PyClassDummySlot }
    };
    let module = if let Some(m) = &attr.module {
        quote! { Some(#m) }
    } else {
        quote! { None }
    };

    // Enforce at compile time that PyGCProtocol is implemented
    let gc_impl = if attr.is_gc {
        let closure_name = format!("__assertion_closure_{}", cls);
        let closure_token = syn::Ident::new(&closure_name, Span::call_site());
        quote! {
            fn #closure_token() {
                use pyo3::class;

                fn _assert_implements_protocol<'p, T: pyo3::class::PyGCProtocol<'p>>() {}
                _assert_implements_protocol::<#cls>();
            }
        }
    } else {
        quote! {}
    };

    let (impl_inventory, iter_py_methods) = match methods_type {
        PyClassMethodsType::Specialization => (None, quote! { collector.py_methods().iter() }),
        PyClassMethodsType::Inventory => (
            Some(impl_methods_inventory(&cls)),
            quote! {
                pyo3::inventory::iter::<<Self as pyo3::class::impl_::HasMethodsInventory>::Methods>
                    .into_iter()
                    .flat_map(pyo3::class::impl_::PyMethodsInventory::get)
            },
        ),
    };

    let base = &attr.base;
    let base_nativetype = if attr.has_extends {
        quote! { <Self::BaseType as pyo3::class::impl_::PyClassBaseType>::BaseNativeType }
    } else {
        quote! { pyo3::PyAny }
    };

    // If #cls is not extended type, we allow Self->PyObject conversion
    let into_pyobject = if !attr.has_extends {
        quote! {
            impl pyo3::IntoPy<pyo3::PyObject> for #cls {
                fn into_py(self, py: pyo3::Python) -> pyo3::PyObject {
                    pyo3::IntoPy::into_py(pyo3::Py::new(py, self).unwrap(), py)
                }
            }
        }
    } else {
        quote! {}
    };

    let thread_checker = if attr.has_unsendable {
        quote! { pyo3::class::impl_::ThreadCheckerImpl<#cls> }
    } else if attr.has_extends {
        quote! {
            pyo3::class::impl_::ThreadCheckerInherited<#cls, <#cls as pyo3::class::impl_::PyClassImpl>::BaseType>
        }
    } else {
        quote! { pyo3::class::impl_::ThreadCheckerStub<#cls> }
    };

    let is_gc = attr.is_gc;
    let is_basetype = attr.is_basetype;
    let is_subclass = attr.has_extends;

    Ok(quote! {
        unsafe impl pyo3::type_object::PyTypeInfo for #cls {
            type AsRefTarget = pyo3::PyCell<Self>;

            const NAME: &'static str = #cls_name;
            const MODULE: Option<&'static str> = #module;

            #[inline]
            fn type_object_raw(py: pyo3::Python) -> *mut pyo3::ffi::PyTypeObject {
                use pyo3::type_object::LazyStaticType;
                static TYPE_OBJECT: LazyStaticType = LazyStaticType::new();
                TYPE_OBJECT.get_or_init::<Self>(py)
            }
        }

        impl pyo3::PyClass for #cls {
            type Dict = #dict;
            type WeakRef = #weakref;
            type BaseNativeType = #base_nativetype;
        }

        impl<'a> pyo3::derive_utils::ExtractExt<'a> for &'a #cls
        {
            type Target = pyo3::PyRef<'a, #cls>;
        }

        impl<'a> pyo3::derive_utils::ExtractExt<'a> for &'a mut #cls
        {
            type Target = pyo3::PyRefMut<'a, #cls>;
        }

        #into_pyobject

        #impl_inventory

        impl pyo3::class::impl_::PyClassImpl for #cls {
            const DOC: &'static str = #doc;
            const IS_GC: bool = #is_gc;
            const IS_BASETYPE: bool = #is_basetype;
            const IS_SUBCLASS: bool = #is_subclass;

            type Layout = PyCell<Self>;
            type BaseType = #base;
            type ThreadChecker = #thread_checker;

            fn for_each_method_def(visitor: &mut dyn FnMut(&pyo3::class::PyMethodDefType)) {
                use pyo3::class::impl_::*;
                let collector = PyClassImplCollector::<Self>::new();
                #iter_py_methods
                    .chain(collector.py_class_descriptors())
                    .chain(collector.object_protocol_methods())
                    .chain(collector.async_protocol_methods())
                    .chain(collector.context_protocol_methods())
                    .chain(collector.descr_protocol_methods())
                    .chain(collector.mapping_protocol_methods())
                    .chain(collector.number_protocol_methods())
                    .for_each(visitor)
            }
            fn get_new() -> Option<pyo3::ffi::newfunc> {
                use pyo3::class::impl_::*;
                let collector = PyClassImplCollector::<Self>::new();
                collector.new_impl()
            }
            fn get_call() -> Option<pyo3::ffi::PyCFunctionWithKeywords> {
                use pyo3::class::impl_::*;
                let collector = PyClassImplCollector::<Self>::new();
                collector.call_impl()
            }

            fn for_each_proto_slot(visitor: &mut dyn FnMut(&pyo3::ffi::PyType_Slot)) {
                // Implementation which uses dtolnay specialization to load all slots.
                use pyo3::class::impl_::*;
                let collector = PyClassImplCollector::<Self>::new();
                collector.object_protocol_slots()
                    .iter()
                    .chain(collector.number_protocol_slots())
                    .chain(collector.iter_protocol_slots())
                    .chain(collector.gc_protocol_slots())
                    .chain(collector.descr_protocol_slots())
                    .chain(collector.mapping_protocol_slots())
                    .chain(collector.sequence_protocol_slots())
                    .chain(collector.async_protocol_slots())
                    .chain(collector.buffer_protocol_slots())
                    .for_each(visitor);
            }

            fn get_buffer() -> Option<&'static pyo3::class::impl_::PyBufferProcs> {
                use pyo3::class::impl_::*;
                let collector = PyClassImplCollector::<Self>::new();
                collector.buffer_procs()
            }
        }

        #extra

        #gc_impl
    })
}

fn impl_descriptors(
    cls: &syn::Type,
    descriptors: Vec<(syn::Field, Vec<FnType>)>,
) -> syn::Result<TokenStream> {
    let py_methods: Vec<TokenStream> = descriptors
        .iter()
        .flat_map(|(field, fns)| {
            fns.iter()
                .map(|desc| {
                    let doc = utils::get_doc(&field.attrs, None, true)
                        .unwrap_or_else(|_| syn::LitStr::new("", Span::call_site()));
                    let property_type = PropertyType::Descriptor(
                        field.ident.as_ref().ok_or_else(
                            || err_spanned!(field.span() => "`#[pyo3(get, set)]` is not supported on tuple struct fields")
                        )?
                    );
                    match desc {
                        FnType::Getter(self_ty) => {
                            impl_py_getter_def(cls, property_type, self_ty, &doc)
                        }
                        FnType::Setter(self_ty) => {
                            impl_py_setter_def(cls, property_type, self_ty, &doc)
                        }
                        _ => unreachable!(),
                    }
                })
                .collect::<Vec<syn::Result<TokenStream>>>()
        })
        .collect::<syn::Result<_>>()?;

    Ok(quote! {
        impl pyo3::class::impl_::PyClassDescriptors<#cls>
            for pyo3::class::impl_::PyClassImplCollector<#cls>
        {
            fn py_class_descriptors(self) -> &'static [pyo3::class::methods::PyMethodDefType] {
                static METHODS: &[pyo3::class::methods::PyMethodDefType] = &[#(#py_methods),*];
                METHODS
            }
        }
    })
}
