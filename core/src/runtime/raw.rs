use std::{
    ffi::CString,
    mem,
    panic::{self, AssertUnwindSafe},
    ptr::NonNull,
    result::Result as StdResult,
};

#[cfg(feature = "allocator")]
use crate::allocator::{Allocator, AllocatorHolder};
#[cfg(feature = "loader")]
use crate::loader::{Loader, LoaderHolder, Resolver};
use crate::{
    qjs::{self, size_t},
    Error, Result,
};

use super::{opaque::Opaque, InterruptHandler};

#[derive(Debug)]
pub(crate) struct RawRuntime {
    pub(crate) rt: NonNull<qjs::JSRuntime>,

    // To keep rt info alive for the entire duration of the lifetime of rt
    #[allow(dead_code)]
    pub info: Option<CString>,

    #[cfg(feature = "allocator")]
    #[allow(dead_code)]
    pub allocator: Option<AllocatorHolder>,
    #[cfg(feature = "loader")]
    #[allow(dead_code)]
    pub loader: Option<LoaderHolder>,
}

#[cfg(feature = "parallel")]
unsafe impl Send for RawRuntime {}

impl Drop for RawRuntime {
    fn drop(&mut self) {
        unsafe {
            let ptr = qjs::JS_GetRuntimeOpaque(self.rt.as_ptr());
            let mut opaque: Box<Opaque> = Box::from_raw(ptr as *mut _);
            opaque.clear();
            qjs::JS_FreeRuntime(self.rt.as_ptr());
            mem::drop(opaque);
        }
    }
}

impl RawRuntime {
    pub unsafe fn new(opaque: Opaque<'static>) -> Result<Self> {
        #[cfg(not(feature = "rust-alloc"))]
        return Self::new_base(opaque);

        #[cfg(feature = "rust-alloc")]
        Self::new_with_allocator(opaque, crate::allocator::RustAllocator)
    }

    #[allow(dead_code)]
    pub unsafe fn new_base(mut opaque: Opaque<'static>) -> Result<Self> {
        let rt = qjs::JS_NewRuntime();
        let rt = NonNull::new(rt).ok_or(Error::Allocation)?;

        opaque.initialize(rt.as_ptr())?;

        let opaque = Box::into_raw(Box::new(opaque));
        unsafe { qjs::JS_SetRuntimeOpaque(rt.as_ptr(), opaque as *mut _) };

        Ok(RawRuntime {
            rt,
            info: None,
            #[cfg(feature = "allocator")]
            allocator: None,
            #[cfg(feature = "loader")]
            loader: None,
        })
    }

    #[cfg(feature = "allocator")]
    pub unsafe fn new_with_allocator<A>(mut opaque: Opaque<'static>, allocator: A) -> Result<Self>
    where
        A: Allocator + 'static,
    {
        let allocator = AllocatorHolder::new(allocator);
        let functions = AllocatorHolder::functions::<A>();
        let opaque_ptr = allocator.opaque_ptr();

        let rt = qjs::JS_NewRuntime2(&functions, opaque_ptr as _);
        let rt = NonNull::new(rt).ok_or(Error::Allocation)?;

        opaque.initialize(rt.as_ptr())?;

        let opaque = Box::into_raw(Box::new(opaque));
        unsafe { qjs::JS_SetRuntimeOpaque(rt.as_ptr(), opaque as *mut _) };

        Ok(RawRuntime {
            rt,
            info: None,
            allocator: Some(allocator),
            #[cfg(feature = "loader")]
            loader: None,
        })
    }

    pub fn update_stack_top(&self) {
        #[cfg(feature = "parallel")]
        unsafe {
            qjs::JS_UpdateStackTop(self.rt.as_ptr());
        }
    }

    pub fn get_opaque<'js>(&self) -> &Opaque<'js> {
        unsafe { &*(qjs::JS_GetRuntimeOpaque(self.rt.as_ptr()) as *mut _) }
    }

    pub fn is_job_pending(&self) -> bool {
        0 != unsafe { qjs::JS_IsJobPending(self.rt.as_ptr()) }
    }

    pub fn execute_pending_job(&mut self) -> StdResult<bool, *mut qjs::JSContext> {
        let mut ctx_ptr = mem::MaybeUninit::<*mut qjs::JSContext>::uninit();
        let result = unsafe { qjs::JS_ExecutePendingJob(self.rt.as_ptr(), ctx_ptr.as_mut_ptr()) };
        if result == 0 {
            // no jobs executed
            return Ok(false);
        }
        if result == 1 {
            // single job executed
            return Ok(true);
        }
        Err(unsafe { ctx_ptr.assume_init() })
    }

    #[cfg(feature = "loader")]
    pub unsafe fn set_loader<R, L>(&mut self, resolver: R, loader: L)
    where
        R: Resolver + 'static,
        L: Loader + 'static,
    {
        let loader = LoaderHolder::new(resolver, loader);
        loader.set_to_runtime(self.rt.as_ptr());
        self.loader = Some(loader);
    }

    /// Set the info of the runtime
    pub unsafe fn set_info(&mut self, info: CString) {
        unsafe { qjs::JS_SetRuntimeInfo(self.rt.as_ptr(), info.as_ptr()) };
        self.info = Some(info);
    }

    /// Set a limit on the max amount of memory the runtime will use.
    ///
    /// Setting the limit to 0 is equivalent to unlimited memory.
    ///
    /// Note that is a Noop when a custom allocator is being used,
    /// as is the case for the "rust-alloc" or "allocator" features.
    pub unsafe fn set_memory_limit(&mut self, limit: usize) {
        let limit: size_t = limit.try_into().unwrap_or(size_t::MAX);
        qjs::JS_SetMemoryLimit(self.rt.as_ptr(), limit)
    }

    /// Set a limit on the max size of stack the runtime will use.
    ///
    /// The default values is 256x1024 bytes.
    pub unsafe fn set_max_stack_size(&mut self, limit: usize) {
        let limit: size_t = limit.try_into().unwrap_or(size_t::MAX);
        qjs::JS_SetMaxStackSize(self.rt.as_ptr(), limit);
    }

    /// Set a memory threshold for garbage collection.
    pub unsafe fn set_gc_threshold(&self, threshold: usize) {
        qjs::JS_SetGCThreshold(self.rt.as_ptr(), threshold as _);
    }

    /// Manually run the garbage collection.
    ///
    /// Most of QuickJS values are reference counted and
    /// will automatically free themselves when they have no more
    /// references. The garbage collector is only for collecting
    /// cyclic references.
    pub unsafe fn run_gc(&mut self) {
        qjs::JS_RunGC(self.rt.as_ptr());
    }

    /// Get memory usage stats
    pub unsafe fn memory_usage(&mut self) -> qjs::JSMemoryUsage {
        let mut stats = mem::MaybeUninit::uninit();
        qjs::JS_ComputeMemoryUsage(self.rt.as_ptr(), stats.as_mut_ptr());
        stats.assume_init()
    }

    /// Set a closure which is regularly called by the engine when it is executing code.
    /// If the provided closure returns `true` the interpreter will raise and uncatchable
    /// exception and return control flow to the caller.
    pub unsafe fn set_interrupt_handler(&mut self, handler: Option<InterruptHandler>) {
        unsafe extern "C" fn interrupt_handler_trampoline(
            _rt: *mut qjs::JSRuntime,
            opaque: *mut ::std::os::raw::c_void,
        ) -> ::std::os::raw::c_int {
            // This should be safe as the value is set below to a non-null pointer.
            let opaque = NonNull::new_unchecked(opaque).cast::<Opaque>();

            let catch_unwind = panic::catch_unwind(AssertUnwindSafe(move || {
                opaque.as_ref().run_interrupt_handler()
            }));
            let should_interrupt = match catch_unwind {
                Ok(should_interrupt) => should_interrupt,
                Err(panic) => {
                    opaque.as_ref().set_panic(panic);
                    // Returning true here will cause the interpreter to raise an un-catchable exception.
                    // The Rust code that is running the interpreter will see that exception and continue
                    // the panic handling. See crate::result::{handle_exception, handle_panic} for details.
                    true
                }
            };
            should_interrupt as _
        }

        qjs::JS_SetInterruptHandler(
            self.rt.as_ptr(),
            handler.as_ref().map(|_| interrupt_handler_trampoline as _),
            qjs::JS_GetRuntimeOpaque(self.rt.as_ptr()),
        );
        self.get_opaque().set_interrupt_handler(handler);
    }
}
