use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::{CStr, CString},
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::{qjs, Atom, Ctx, Error, FromAtom, FromJs, IntoJs, Result, Value};

struct ModuleBuildData {
    name: Vec<u8>,
    source: Vec<u8>,
}

/// A struct for loading multiple modules at once safely.
///
/// Modules are built in two steps, compile and evaluate.
/// During evalation a module might import another module, if no such compiled module exist the
/// evaluation fails.
///
/// This struct allows one to first compile all possible modules and then evaluate them allowing
/// modules to import eachother.
pub struct ModuleBuilder {
    modules: Vec<ModuleBuildData>,
}

impl ModuleBuilder {
    pub fn new() -> Self {
        ModuleBuilder {
            modules: Vec::new(),
        }
    }

    pub fn compile<N, S>(mut self, name: N, source: S) -> Result<Self>
    where
        N: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        self.with_compile(name, source)?;
        Ok(self)
    }

    pub fn with_compile<N, S>(&mut self, name: N, source: S) -> Result<&mut Self>
    where
        N: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        self.modules.push(ModuleBuildData {
            name: name.into(),
            source: source.into(),
        });
        Ok(self)
    }

    pub fn eval<'js>(self, ctx: Ctx<'js>) -> Result<Vec<Module<'js>>> {
        let mut modules = Vec::with_capacity(self.modules.len());
        for m in self
            .modules
            .into_iter()
            .map(|x| unsafe { Module::unsafe_define(ctx, x.name, x.source) })
        {
            modules.push(m?);
        }

        for m in modules.iter() {
            // Safety:
            // This is save usage of the modules since if any fail to evaluate we immediatly return
            // and drop all modules
            unsafe { m.eval()? }
        }
        // All modules evaluated without error so they are all still alive.
        Ok(modules)
    }
}

impl Default for ModuleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Definitions {
    definitions: HashSet<Cow<'static, CStr>>,
}

impl Definitions {
    pub(crate) fn new() -> Self {
        Definitions {
            definitions: HashSet::new(),
        }
    }
    pub fn define<N>(&mut self, name: N) -> Result<&mut Self>
    where
        N: Into<Vec<u8>>,
    {
        self.definitions.insert(Cow::Owned(CString::new(name)?));
        Ok(self)
    }

    pub fn define_static(&mut self, name: &'static CStr) -> Result<&mut Self> {
        self.definitions.insert(Cow::Borrowed(name));
        Ok(self)
    }

    pub(crate) unsafe fn apply(self, ctx: Ctx<'_>, module: Module) -> Result<()> {
        for k in self.definitions {
            let ptr = match k {
                Cow::Borrowed(x) => x.as_ptr(),
                Cow::Owned(x) => x.into_raw(),
            };
            let res = unsafe {
                qjs::JS_AddModuleExport(ctx.as_ptr(), module.as_module_def().as_ptr(), ptr)
            };
            if res < 0 {
                return Err(Error::Allocation);
            }
        }
        Ok(())
    }
}

struct Export<'js> {
    name: CString,
    value: Value<'js>,
}

/// A struct used to load the exports of a module.
///
/// Used in the `ModuleDef::load`.
pub struct Exports<'js> {
    ctx: Ctx<'js>,
    exports: Vec<Export<'js>>,
}

impl<'js> Exports<'js> {
    pub(crate) fn new(ctx: Ctx<'js>) -> Self {
        Exports {
            ctx,
            exports: Vec::new(),
        }
    }

    /// Export a new value from the module.
    pub fn export<N: Into<Vec<u8>>, T: IntoJs<'js>>(
        &mut self,
        name: N,
        value: T,
    ) -> Result<&mut Self> {
        let name = CString::new(name.into())?;
        let ctx = self.ctx;
        let value = value.into_js(ctx)?;
        self.export_value(name, value)
    }

    /// Export a new value from the module.
    pub fn export_value(&mut self, name: CString, value: Value<'js>) -> Result<&mut Self> {
        self.exports.push(Export { name, value });
        Ok(self)
    }

    pub(crate) unsafe fn apply(self, module: Module) -> Result<()> {
        for export in self.exports {
            let name = export.name;
            let value = export.value;

            let res = unsafe {
                // Ownership of name is retained
                // Ownership of value is transfered.
                qjs::JS_SetModuleExport(
                    self.ctx.as_ptr(),
                    module.as_module_def().as_ptr(),
                    name.as_ref().as_ptr(),
                    value.into_js_value(),
                )
            };

            // previous checks and the fact that we also previously added the export should ensure
            // that the only error can be an allocation error.
            if res < 0 {
                return Err(Error::Allocation);
            }
        }
        Ok(())
    }
}

/// A javascript module.
#[derive(Clone, Copy)]
pub struct Module<'js> {
    ctx: Ctx<'js>,
    /// Module lifetime, behave differently then normal lifetimes.
    /// A module lifes for the entire lifetime of the runtime,
    /// So no duplication and
    module: NonNull<qjs::JSModuleDef>,
}

/// Module definition trait
pub trait ModuleDef {
    fn define(define: &mut Definitions) -> Result<()> {
        let _ = define;
        Ok(())
    }

    /// The exports should be added here
    fn export<'js>(_ctx: Ctx<'js>, exports: &mut Exports<'js>) -> Result<()> {
        let _ = exports;
        Ok(())
    }
}

impl<'js> Module<'js> {
    pub(crate) fn from_module_def(ctx: Ctx<'js>, def: NonNull<qjs::JSModuleDef>) -> Self {
        Module { ctx, module: def }
    }

    pub(crate) fn as_module_def(&self) -> NonNull<qjs::JSModuleDef> {
        self.module
    }

    /// Defines a new JS module in the context.
    ///
    /// This function doesn't return a module since holding on to unevaluated modules is unsafe.
    /// If you need to hold onto unsafe modules use the `unsafe_define` functions.
    ///
    /// It is unsafe to hold onto unevaluated modules across this call.
    pub fn define<N, S>(ctx: Ctx<'js>, name: N, source: S) -> Result<()>
    where
        N: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        unsafe { Self::unsafe_define(ctx, name, source)? };
        Ok(())
    }

    /// Creates a new module from JS source, and evaluates it.
    ///
    /// It is unsafe to hold onto unevaluated modules across this call.
    pub fn instanciate<N, S>(ctx: Ctx<'js>, name: N, source: S) -> Result<Module<'js>>
    where
        N: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        let module = unsafe { Self::unsafe_define(ctx, name, source)? };
        unsafe { module.eval()? };
        Ok(module)
    }

    /// Defines a module in the runtime.
    ///
    /// This function doesn't return a module since holding on to unevaluated modules is unsafe.
    /// If you need to hold onto unsafe modules use the `unsafe_define_def` functions.
    ///
    /// It is unsafe to hold onto unevaluated modules across this call.
    pub fn define_def<D, N>(ctx: Ctx<'js>, name: N) -> Result<()>
    where
        N: Into<Vec<u8>>,
        D: ModuleDef,
    {
        unsafe { Self::unsafe_define_def::<D, _>(ctx, name)? };
        Ok(())
    }

    /// Defines a module in the runtime and evaluates it.
    ///
    /// It is unsafe to hold onto unevaluated modules across this call.
    pub fn instanciate_def<D, N>(ctx: Ctx<'js>, name: N) -> Result<Module<'js>>
    where
        N: Into<Vec<u8>>,
        D: ModuleDef,
    {
        let module = unsafe { Self::unsafe_define_def::<D, _>(ctx, name)? };
        unsafe { module.eval()? };
        Ok(module)
    }

    /// Returns the name of the module
    pub fn name<N>(&self) -> Result<N>
    where
        N: FromAtom<'js>,
    {
        let name = unsafe {
            Atom::from_atom_val(
                self.ctx,
                qjs::JS_GetModuleName(self.ctx.as_ptr(), self.as_module_def().as_ptr()),
            )
        };
        N::from_atom(name)
    }

    /// Return the `import.meta` object of a module
    pub fn meta<T>(&self) -> Result<T>
    where
        T: FromJs<'js>,
    {
        let meta = unsafe {
            Value::from_js_value(
                self.ctx,
                self.ctx.handle_exception(qjs::JS_GetImportMeta(
                    self.ctx.as_ptr(),
                    self.as_module_def().as_ptr(),
                ))?,
            )
        };
        T::from_js(self.ctx, meta)
    }

    /// Creates a new module from JS source but doesnt evaluates the module.
    ///
    /// # Safety
    /// It is unsafe to use an unevaluated for anything other then evaluating it with
    /// `Module::eval`.
    ///
    /// Quickjs frees all unevaluated modules if any error happens while compiling or evaluating a
    /// module. If any call to either `Module::new` or `Module::eval` fails it is undefined
    /// behaviour to use any unevaluated modules.
    pub unsafe fn unsafe_define<N, S>(ctx: Ctx<'js>, name: N, source: S) -> Result<Module<'js>>
    where
        N: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        let name = CString::new(name)?;
        let flag =
            qjs::JS_EVAL_TYPE_MODULE | qjs::JS_EVAL_FLAG_STRICT | qjs::JS_EVAL_FLAG_COMPILE_ONLY;

        let module = unsafe { ctx.eval_raw(source, name.as_c_str(), flag as i32)? };
        let module = ctx.handle_exception(module)?;
        debug_assert_eq!(qjs::JS_TAG_MODULE, unsafe { qjs::JS_VALUE_GET_TAG(module) });
        let module = qjs::JS_VALUE_GET_PTR(module).cast::<qjs::JSModuleDef>();
        // Quickjs should throw an exception on allocation errors
        // So this should always be non-null.
        let module = NonNull::new(module).unwrap();

        Ok(Module { ctx, module })
    }

    /// Creates a new module from JS source but doesnt evaluates the module.
    ///
    /// # Safety
    /// It is unsafe to use an unevaluated for anything other then evaluating it with
    /// `Module::eval`.
    ///
    /// Quickjs frees all unevaluated modules if any error happens while compiling or evaluating a
    /// module. If any call to either `Module::new` or `Module::eval` fails it is undefined
    /// behaviour to use any unevaluated modules.
    pub unsafe fn unsafe_define_def<D, N>(ctx: Ctx<'js>, name: N) -> Result<Module<'js>>
    where
        N: Into<Vec<u8>>,
        D: ModuleDef,
    {
        let name = CString::new(name)?;

        let mut defs = Definitions::new();
        D::define(&mut defs)?;

        let ptr =
            unsafe { qjs::JS_NewCModule(ctx.as_ptr(), name.as_ptr(), Some(Self::eval_fn::<D>)) };

        let ptr = NonNull::new(ptr).ok_or(Error::Allocation)?;
        let module = Module::from_module_def(ctx, ptr);
        // Safety: Safe because this is a newly created
        unsafe { defs.apply(ctx, module)? };
        Ok(module)
    }

    unsafe extern "C" fn eval_fn<D>(
        ctx: *mut qjs::JSContext,
        ptr: *mut qjs::JSModuleDef,
    ) -> qjs::c_int
    where
        D: ModuleDef,
    {
        let ctx = Ctx::from_ptr(ctx);
        // Should never be null
        debug_assert_ne!(ptr, ptr::null_mut());
        let ptr = NonNull::new_unchecked(ptr);
        let module = Self::from_module_def(ctx, ptr);
        let mut exports = Exports::new(ctx);
        match D::export(ctx, &mut exports).and_then(|_| exports.apply(module)) {
            Ok(_) => 0,
            Err(error) => {
                error.throw(ctx);
                -1
            }
        }
    }

    /// Evaluates an unevaluated module.
    ///
    /// # Safety
    /// This function should only be called on unevaluated modules.
    ///
    /// Quickjs frees all unevaluated modules if any error happens while compiling or evaluating a
    /// module. If any call to either `Module::new` or `Module::eval` fails it is undefined
    /// behaviour to use any unevaluated modules.
    ///
    /// Prefer the use of either `ModuleBuilder` or `Module::new`.
    pub unsafe fn eval(self) -> Result<()> {
        unsafe {
            let value = qjs::JS_MKPTR(qjs::JS_TAG_MODULE, self.module.as_ptr().cast());
            // JS_EvalFunction `free's` the module so we should dup first
            let ret = qjs::JS_EvalFunction(self.ctx.as_ptr(), qjs::JS_DupValue(value));
            self.ctx.handle_exception(ret)?;
        }
        Ok(())
    }
}

#[cfg(feature = "exports")]
impl<'js> Module<'js> {
    /// Return exported value by name
    pub fn get<N, T>(&self, name: N) -> Result<T>
    where
        N: AsRef<str>,
        T: FromJs<'js>,
    {
        let name = CString::new(name.as_ref())?;
        let value = unsafe {
            Value::from_js_value(
                self.ctx,
                self.ctx.handle_exception(qjs::JS_GetModuleExport(
                    self.ctx.as_ptr(),
                    self.as_module_def().as_ptr(),
                    name.as_ptr(),
                ))?,
            )
        };
        T::from_js(self.ctx, value)
    }

    /// Returns a iterator over the exported names of the module export.
    #[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "exports")))]
    pub fn names<N>(self) -> ExportNamesIter<'js, N>
    where
        N: FromAtom<'js>,
    {
        ExportNamesIter {
            module: self,
            count: unsafe { qjs::JS_GetModuleExportEntriesCount(self.as_module_def().as_ptr()) },
            index: 0,
            marker: PhantomData,
        }
    }

    /// Returns a iterator over the items the module export.
    #[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "exports")))]
    pub fn entries<N, T>(self) -> ExportEntriesIter<'js, N, T>
    where
        N: FromAtom<'js>,
        T: FromJs<'js>,
    {
        ExportEntriesIter {
            module: self,
            count: unsafe { qjs::JS_GetModuleExportEntriesCount(self.as_module_def().as_ptr()) },
            index: 0,
            marker: PhantomData,
        }
    }

    #[doc(hidden)]
    pub unsafe fn dump_exports(&self) {
        let ptr = self.as_module_def().as_ptr();
        let count = qjs::JS_GetModuleExportEntriesCount(ptr);
        for i in 0..count {
            let atom_name = Atom::from_atom_val(
                self.ctx,
                qjs::JS_GetModuleExportEntryName(self.ctx.as_ptr(), ptr, i),
            );
            println!("{}", atom_name.to_string().unwrap());
        }
    }
}

/// An iterator over the items exported out a module
#[cfg(feature = "exports")]
#[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "exports")))]
pub struct ExportNamesIter<'js, N> {
    module: Module<'js>,
    count: i32,
    index: i32,
    marker: PhantomData<N>,
}

#[cfg(feature = "exports")]
impl<'js, N> Iterator for ExportNamesIter<'js, N>
where
    N: FromAtom<'js>,
{
    type Item = Result<N>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.count {
            return None;
        }
        let ctx = self.module.ctx;
        let ptr = self.module.as_module_def().as_ptr();
        let atom = unsafe {
            let atom_val = qjs::JS_GetModuleExportEntryName(ctx.as_ptr(), ptr, self.index);
            Atom::from_atom_val(ctx, atom_val)
        };
        self.index += 1;
        Some(N::from_atom(atom))
    }
}

/// An iterator over the items exported out a module
#[cfg(feature = "exports")]
#[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "exports")))]
pub struct ExportEntriesIter<'js, N, T> {
    module: Module<'js>,
    count: i32,
    index: i32,
    marker: PhantomData<(N, T)>,
}

#[cfg(feature = "exports")]
impl<'js, N, T> Iterator for ExportEntriesIter<'js, N, T>
where
    N: FromAtom<'js>,
    T: FromJs<'js>,
{
    type Item = Result<(N, T)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.count {
            return None;
        }
        let ctx = self.module.ctx;
        let ptr = self.module.as_module_def().as_ptr();
        let name = unsafe {
            let atom_val = qjs::JS_GetModuleExportEntryName(ctx.as_ptr(), ptr, self.index);
            Atom::from_atom_val(ctx, atom_val)
        };
        let value = unsafe {
            let js_val = qjs::JS_GetModuleExportEntry(ctx.as_ptr(), ptr, self.index);
            Value::from_js_value(ctx, js_val)
        };
        self.index += 1;
        Some(N::from_atom(name).and_then(|name| T::from_js(ctx, value).map(|value| (name, value))))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::*;

    pub struct RustModule;

    impl ModuleDef for RustModule {
        fn define(define: &mut Definitions) -> Result<()> {
            define.define_static(CStr::from_bytes_with_nul(b"hello\0")?)?;
            Ok(())
        }

        fn export<'js>(_ctx: Ctx<'js>, exports: &mut Exports<'js>) -> Result<()> {
            exports.export("hello", "world".to_string())?;
            Ok(())
        }
    }

    #[test]
    fn from_rust_def() {
        test_with(|ctx| {
            Module::define_def::<RustModule, _>(ctx, "rust_mod").unwrap();
        })
    }

    #[test]
    fn from_rust_def_eval() {
        test_with(|ctx| {
            Module::instanciate_def::<RustModule, _>(ctx, "rust_mod").unwrap();
        })
    }

    #[test]
    fn import_native() {
        test_with(|ctx| {
            Module::define_def::<RustModule, _>(ctx, "rust_mod").unwrap();
            ctx.compile(
                "test",
                r#"
                import { hello } from "rust_mod";

                globalThis.hello = hello;
            "#,
            )
            .unwrap();
            let text = ctx
                .globals()
                .get::<_, String>("hello")
                .unwrap()
                .to_string()
                .unwrap();
            assert_eq!(text.as_str(), "world");
        })
    }

    #[test]
    fn from_javascript() {
        test_with(|ctx| {
            let module: Module = ctx
                .compile(
                    "Test",
                    r#"
            export var a = 2;
            export function foo(){ return "bar"}
            export class Baz{
                quel = 3;
                constructor(){
                }
            }
                "#,
                )
                .unwrap();

            assert_eq!(module.name::<StdString>().unwrap(), "Test");
            let _ = module.meta::<Object>().unwrap();

            #[cfg(feature = "exports")]
            {
                let names = module.names().collect::<Result<Vec<StdString>>>().unwrap();

                assert_eq!(names[0], "a");
                assert_eq!(names[1], "foo");
                assert_eq!(names[2], "Baz");

                let entries = module
                    .entries()
                    .collect::<Result<Vec<(StdString, Value)>>>()
                    .unwrap();

                assert_eq!(entries[0].0, "a");
                assert_eq!(i32::from_js(ctx, entries[0].1.clone()).unwrap(), 2);
                assert_eq!(entries[1].0, "foo");
                assert_eq!(entries[2].0, "Baz");
            }
        });
    }
}
