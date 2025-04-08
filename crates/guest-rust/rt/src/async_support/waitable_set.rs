//! Low-level FFI-like bindings around `waitable-set` in the canonical ABI.

use std::num::NonZeroU32;

pub struct WaitableSet(NonZeroU32);

impl WaitableSet {
    pub fn new() -> WaitableSet {
        WaitableSet(NonZeroU32::new(unsafe { new() }).unwrap())
    }

    pub fn join(&self, waitable: u32) {
        unsafe { join(waitable, self.0.get()) }
    }

    pub fn remove_waitable_from_all_sets(waitable: u32) {
        unsafe { join(waitable, 0) }
    }

    pub fn wait(&self) -> (u32, u32, u32) {
        unsafe {
            let mut payload = [0; 2];
            let event0 = wait(self.0.get(), &mut payload);
            (event0, payload[0], payload[1])
        }
    }

    pub fn as_raw(&self) -> u32 {
        self.0.get()
    }
}

impl Drop for WaitableSet {
    fn drop(&mut self) {
        unsafe {
            drop(self.0.get());
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn new() -> u32 {
    unreachable!()
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn drop(_: u32) {
    unreachable!()
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn join(_: u32, _: u32) {
    unreachable!()
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn wait(_: u32, _: *mut [u32; 2]) -> u32 {
    unreachable!();
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "$root")]
extern "C" {
    #[link_name = "[waitable-set-new]"]
    fn new() -> u32;
    #[link_name = "[waitable-set-drop]"]
    fn drop(set: u32);
    #[link_name = "[waitable-join]"]
    fn join(waitable: u32, set: u32);
    #[link_name = "[waitable-set-wait]"]
    fn wait(_: u32, _: *mut [u32; 2]) -> u32;
}
