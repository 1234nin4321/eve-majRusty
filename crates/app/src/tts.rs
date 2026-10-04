//! Text-to-speech alerts via SAPI (`SAPI.SpVoice`), spoken on a lazily-started worker thread.
//!
//! The queue/worker machinery is generic over [`Engine`] so it compiles and is tested off Windows;
//! the SAPI engine itself lives in the Windows-only `sapi` module and is driven through the
//! process-wide [`set_voice_settings`], [`speak_alert`] and [`shutdown`] functions.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use eve_maj_core::log::Scope;

const SLOG: Scope = Scope::new("tts");

const WORKER_POLL_MS: u64 = 50;

/// What the worker thread does with each queued command. Every method runs on the worker thread
/// that created the engine.
pub trait Engine {
    fn speak(&mut self, text: &str);
    fn set_volume(&mut self, volume: u8);
    fn set_rate(&mut self, rate: i8);
}

/// Creates the engine on the worker thread; the error is a short name for the log line.
pub type EngineInit<E> = fn() -> Result<E, &'static str>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    // Owned by the queue; the worker drops it after speaking (or shutdown() drops it if still queued).
    Speak(String),
    SetVolume(u8),
    SetRate(i8),
}

struct CommandQueue {
    items: Mutex<VecDeque<Command>>,
}

impl CommandQueue {
    const fn new() -> Self {
        Self { items: Mutex::new(VecDeque::new()) }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Command>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, cmd: Command) {
        self.lock().push_back(cmd);
    }

    fn pop(&self) -> Option<Command> {
        self.lock().pop_front()
    }
}

struct WorkerSlot {
    thread: Option<JoinHandle<()>>,
    thread_failed: bool,
}

/// The command queue plus the worker thread that drains it into an `E`.
pub struct Tts<E> {
    init: EngineInit<E>,
    queue: CommandQueue,
    worker: Mutex<WorkerSlot>,
    should_exit: AtomicBool,
    worker_dead: AtomicBool,
    // The engine only ever exists on the worker thread, so `Tts` is Sync regardless of `E`.
    _engine: PhantomData<fn() -> E>,
}

impl<E: Engine + 'static> Tts<E> {
    pub const fn new(init: EngineInit<E>) -> Self {
        Self {
            init,
            queue: CommandQueue::new(),
            worker: Mutex::new(WorkerSlot { thread: None, thread_failed: false }),
            should_exit: AtomicBool::new(false),
            worker_dead: AtomicBool::new(false),
            _engine: PhantomData,
        }
    }

    fn worker_main(&self) {
        let mut engine = match (self.init)() {
            Ok(engine) => engine,
            Err(err) => {
                SLOG.warn(format_args!("TTS unavailable (SAPI init failed): {err}"));
                self.worker_dead.store(true, Ordering::Release);
                return;
            }
        };
        SLOG.info(format_args!("TTS engine initialized"));

        while !self.should_exit.load(Ordering::Acquire) {
            let Some(cmd) = self.queue.pop() else {
                std::thread::sleep(Duration::from_millis(WORKER_POLL_MS));
                continue;
            };
            match cmd {
                Command::Speak(text) => engine.speak(&text),
                Command::SetVolume(v) => engine.set_volume(v),
                Command::SetRate(r) => engine.set_rate(r),
            }
        }
        // `engine` drops here, on the thread that created it.
    }

    fn lock_worker(&self) -> MutexGuard<'_, WorkerSlot> {
        self.worker.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ensure_worker(&'static self) -> bool {
        let mut slot = self.lock_worker();
        if let Some(thread) = slot.thread.take() {
            if !self.worker_dead.load(Ordering::Acquire) {
                slot.thread = Some(thread);
                return true;
            }
            let _ = thread.join();
            slot.thread_failed = true;
            return false;
        }
        if slot.thread_failed {
            return false;
        }

        match std::thread::Builder::new().spawn(move || self.worker_main()) {
            Ok(thread) => {
                slot.thread = Some(thread);
                true
            }
            Err(err) => {
                SLOG.warn(format_args!("Failed to start TTS worker thread: {err}"));
                slot.thread_failed = true;
                false
            }
        }
    }

    /// Queue a volume/rate change for the worker thread; safe even if TTS hasn't started yet, and silently no-ops if the worker fails to start.
    pub fn set_voice_settings(&'static self, volume: u8, rate: i8) {
        if !self.ensure_worker() {
            return;
        }
        self.queue.push(Command::SetVolume(volume));
        self.queue.push(Command::SetRate(rate));
    }

    /// Queue a short alert phrase to be spoken; returns immediately and speaks in FIFO order on the lazily-started worker thread, no-op if the engine is unavailable.
    pub fn speak_alert(&'static self, text: &str) {
        if !self.ensure_worker() {
            return;
        }
        self.queue.push(Command::Speak(text.to_owned()));
    }

    /// Stop the worker thread, if one was ever started. Call once during app shutdown.
    pub fn shutdown(&self) {
        let Some(thread) = self.lock_worker().thread.take() else {
            return;
        };
        self.should_exit.store(true, Ordering::Release);
        let _ = thread.join();

        while self.queue.pop().is_some() {}
    }
}

#[cfg(windows)]
static SAPI_TTS: Tts<sapi::TtsEngine> = Tts::new(sapi::TtsEngine::init);

/// Queue a volume/rate change for the worker thread; safe even if TTS hasn't started yet, and silently no-ops if the worker fails to start.
#[cfg(windows)]
pub fn set_voice_settings(volume: u8, rate: i8) {
    SAPI_TTS.set_voice_settings(volume, rate);
}

/// Queue a short alert phrase to be spoken; returns immediately and speaks in FIFO order on the lazily-started worker thread, no-op if SAPI is unavailable.
#[cfg(windows)]
pub fn speak_alert(text: &str) {
    SAPI_TTS.speak_alert(text);
}

/// Stop the worker thread, if one was ever started. Call once during app shutdown.
#[cfg(windows)]
pub fn shutdown() {
    SAPI_TTS.shutdown();
}

#[cfg(windows)]
mod sapi {
    use std::ffi::c_void;
    use std::ptr;

    use eve_maj_win::sys::core::{GUID, HRESULT};
    use eve_maj_win::sys::Win32::Foundation::{SysAllocString, SysFreeString};
    use eve_maj_win::sys::Win32::System::Com::{
        CLSIDFromProgID, CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, DISPPARAMS,
    };
    use eve_maj_win::wide;

    use super::{Engine, SLOG};

    const IID_IDISPATCH: GUID = GUID::from_u128(0x00020400_0000_0000_c000_000000000046);
    const IID_NULL: GUID = GUID::from_u128(0);

    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
    // Not the real LOCALE_USER_DEFAULT (0x0400); SpVoice's member names aren't localized so any valid LCID works, and en-US is a safe choice.
    const LCID_EN_US: u32 = 0x0409;

    const DISPATCH_METHOD: u16 = 0x1;
    const DISPATCH_PROPERTYGET: u16 = 0x2;
    const DISPATCH_PROPERTYPUT: u16 = 0x4;
    const DISPID_PROPERTYPUT: i32 = -3;

    const VT_I4: u16 = 3;
    const VT_BSTR: u16 = 8;

    /// Minimal VARIANT: the tag, three reserved words and the payload, padded to the real 24-byte x64 size
    /// so it can be handed to `DISPPARAMS::rgvarg` directly.
    #[repr(C)]
    struct Variant {
        vt: u16,
        reserved1: u16,
        reserved2: u16,
        reserved3: u16,
        payload: u64,
        _pad: usize,
    }

    impl Variant {
        fn from_i32(v: i32) -> Self {
            Self { vt: VT_I4, reserved1: 0, reserved2: 0, reserved3: 0, payload: v as i64 as u64, _pad: 0 }
        }

        fn from_bstr(bstr: *const u16) -> Self {
            Self { vt: VT_BSTR, reserved1: 0, reserved2: 0, reserved3: 0, payload: bstr as usize as u64, _pad: 0 }
        }
    }

    #[repr(C)]
    struct IDispatchVtbl {
        query_interface: unsafe extern "system" fn(*mut IDispatch, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut IDispatch) -> u32,
        release: unsafe extern "system" fn(*mut IDispatch) -> u32,
        get_type_info_count: unsafe extern "system" fn(*mut IDispatch, *mut u32) -> HRESULT,
        get_type_info: unsafe extern "system" fn(*mut IDispatch, u32, u32, *mut *mut c_void) -> HRESULT,
        get_ids_of_names:
            unsafe extern "system" fn(*mut IDispatch, *const GUID, *const *const u16, u32, u32, *mut i32) -> HRESULT,
        #[allow(clippy::type_complexity)]
        invoke: unsafe extern "system" fn(
            *mut IDispatch,
            i32,
            *const GUID,
            u32,
            u16,
            *mut DISPPARAMS,
            *mut Variant,
            *mut c_void,
            *mut u32,
        ) -> HRESULT,
    }

    // Driven via late-bound Invoke rather than a hand-rolled ISpVoice vtable, since IDispatch's layout is fixed while SAPI's real vtable order is easy to mis-transcribe.
    #[repr(C)]
    struct IDispatch {
        vtable: *const IDispatchVtbl,
    }

    /// Owned IDispatch reference; released on drop.
    struct Dispatch(*mut IDispatch);

    impl Drop for Dispatch {
        fn drop(&mut self) {
            // SAFETY: self.0 is a live interface pointer we hold one reference to.
            unsafe { ((*(*self.0).vtable).release)(self.0) };
        }
    }

    impl Dispatch {
        fn vtbl(&self) -> &IDispatchVtbl {
            // SAFETY: self.0 is a live interface pointer whose first field is its vtable.
            unsafe { &*(*self.0).vtable }
        }

        fn get_disp_id(&self, name: &str) -> Result<i32, &'static str> {
            let name_w = wide(name);
            let names = [name_w.as_ptr()];
            let mut dispids = [0i32];
            // SAFETY: one NUL-terminated name in, one DISPID out.
            let hr = unsafe {
                (self.vtbl().get_ids_of_names)(self.0, &IID_NULL, names.as_ptr(), 1, LCID_EN_US, dispids.as_mut_ptr())
            };
            if hr < 0 {
                return Err("GetIDsOfNamesFailed");
            }
            Ok(dispids[0])
        }

        fn invoke_method(&self, dispid: i32, args: &mut [Variant]) -> Result<(), &'static str> {
            let mut params = DISPPARAMS {
                rgvarg: args.as_mut_ptr().cast(),
                rgdispidNamedArgs: ptr::null_mut(),
                cArgs: args.len() as u32,
                cNamedArgs: 0,
            };
            // SAPI's SpVoice rejects a bare DISPATCH_METHOD flag; combine with DISPATCH_PROPERTYGET so Invoke can disambiguate a method call from an array-element get.
            // SAFETY: params points at `args`, which outlives the call.
            let hr = unsafe {
                (self.vtbl().invoke)(
                    self.0,
                    dispid,
                    &IID_NULL,
                    LCID_EN_US,
                    DISPATCH_METHOD | DISPATCH_PROPERTYGET,
                    &mut params,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if hr < 0 {
                return Err("InvokeFailed");
            }
            Ok(())
        }

        fn invoke_property_put(&self, dispid: i32, value: i32) -> Result<(), &'static str> {
            let mut args = [Variant::from_i32(value)];
            let mut named_args = [DISPID_PROPERTYPUT];
            let mut params = DISPPARAMS {
                rgvarg: args.as_mut_ptr().cast(),
                rgdispidNamedArgs: named_args.as_mut_ptr(),
                cArgs: 1,
                cNamedArgs: 1,
            };
            // SAFETY: params points at locals that outlive the call.
            let hr = unsafe {
                (self.vtbl().invoke)(
                    self.0,
                    dispid,
                    &IID_NULL,
                    LCID_EN_US,
                    DISPATCH_PROPERTYPUT,
                    &mut params,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if hr < 0 {
                return Err("InvokeFailed");
            }
            Ok(())
        }
    }

    /// Balances a successful CoInitializeEx on this thread; dropped after the dispatch it guards.
    struct ComApartment;

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: paired with the CoInitializeEx that created this guard, on the same thread.
            unsafe { CoUninitialize() };
        }
    }

    // COM objects are apartment-affine, so every method here (not just speak) must run on the same STA thread that created dispatch.
    pub(super) struct TtsEngine {
        // Field order matters: dispatch is released before the apartment is torn down.
        dispatch: Dispatch,
        speak_dispid: i32,
        rate_dispid: i32,
        volume_dispid: i32,
        _com: ComApartment,
    }

    impl TtsEngine {
        pub(super) fn init() -> Result<Self, &'static str> {
            // SAFETY: plain COM initialization for the calling thread.
            let hr_init = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
            // 0x1 is S_FALSE (already initialized on this thread), which is fine.
            if hr_init < 0 && hr_init != 0x1 {
                return Err("ComInitFailed");
            }
            let com = ComApartment;

            let mut clsid = GUID::from_u128(0);
            let progid = wide("SAPI.SpVoice");
            // SAFETY: progid is NUL-terminated; clsid is a valid out pointer.
            if unsafe { CLSIDFromProgID(progid.as_ptr(), &mut clsid) } < 0 {
                return Err("SapiNotAvailable");
            }

            let mut dispatch_ptr: *mut c_void = ptr::null_mut();
            // SAFETY: valid CLSID/IID and out pointer.
            let create_hr = unsafe {
                CoCreateInstance(&clsid, ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_IDISPATCH, &mut dispatch_ptr)
            };
            if create_hr < 0 || dispatch_ptr.is_null() {
                return Err("CreateSpVoiceFailed");
            }

            let dispatch = Dispatch(dispatch_ptr.cast());

            let speak_dispid = dispatch.get_disp_id("Speak")?;
            let rate_dispid = dispatch.get_disp_id("Rate")?;
            let volume_dispid = dispatch.get_disp_id("Volume")?;

            Ok(Self { dispatch, speak_dispid, rate_dispid, volume_dispid, _com: com })
        }
    }

    impl Engine for TtsEngine {
        fn set_volume(&mut self, volume: u8) {
            if let Err(err) = self.dispatch.invoke_property_put(self.volume_dispid, i32::from(volume)) {
                SLOG.warn(format_args!("Failed to set TTS volume: {err}"));
            }
        }

        fn set_rate(&mut self, rate: i8) {
            if let Err(err) = self.dispatch.invoke_property_put(self.rate_dispid, i32::from(rate)) {
                SLOG.warn(format_args!("Failed to set TTS rate: {err}"));
            }
        }

        // Blocks until finished: SpVoice's Invoke only accepts the reduced-arity Text-only call, not one with an explicit Flags arg, so this must never run on the main thread.
        fn speak(&mut self, text: &str) {
            let text_w = wide(text);

            // SAFETY: text_w is NUL-terminated.
            let bstr = unsafe { SysAllocString(text_w.as_ptr()) };
            if bstr.is_null() {
                SLOG.warn(format_args!("SysAllocString failed for TTS text"));
                return;
            }

            let mut args = [Variant::from_bstr(bstr)];
            if let Err(err) = self.dispatch.invoke_method(self.speak_dispid, &mut args) {
                SLOG.warn(format_args!("Failed to speak TTS alert: {err}"));
            }
            // SAFETY: bstr came from SysAllocString and Invoke does not take ownership of in-args.
            unsafe { SysFreeString(bstr) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    static FIFO_LOG: Mutex<Vec<Command>> = Mutex::new(Vec::new());
    static FIFO_INIT_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

    struct FifoEngine;
    impl FifoEngine {
        fn init() -> Result<Self, &'static str> {
            *FIFO_INIT_THREAD.lock().unwrap() = Some(std::thread::current().id());
            Ok(Self)
        }
        fn record(cmd: Command) {
            // Every command must run on the thread that created the engine.
            assert_eq!(*FIFO_INIT_THREAD.lock().unwrap(), Some(std::thread::current().id()));
            FIFO_LOG.lock().unwrap().push(cmd);
        }
    }
    impl Engine for FifoEngine {
        fn speak(&mut self, text: &str) {
            Self::record(Command::Speak(text.to_owned()));
        }
        fn set_volume(&mut self, volume: u8) {
            Self::record(Command::SetVolume(volume));
        }
        fn set_rate(&mut self, rate: i8) {
            Self::record(Command::SetRate(rate));
        }
    }

    #[test]
    fn commands_run_in_fifo_order_on_worker_thread() {
        static TTS: Tts<FifoEngine> = Tts::new(FifoEngine::init);
        TTS.set_voice_settings(80, -3);
        TTS.speak_alert("first");
        TTS.speak_alert("second");
        assert!(wait_for(|| FIFO_LOG.lock().unwrap().len() == 4));
        assert_eq!(
            *FIFO_LOG.lock().unwrap(),
            vec![
                Command::SetVolume(80),
                Command::SetRate(-3),
                Command::Speak("first".into()),
                Command::Speak("second".into()),
            ]
        );
        assert_ne!(*FIFO_INIT_THREAD.lock().unwrap(), Some(std::thread::current().id()));
        TTS.shutdown();
        assert!(TTS.lock_worker().thread.is_none());
    }

    struct FailingEngine;
    impl FailingEngine {
        fn init() -> Result<Self, &'static str> {
            Err("SapiNotAvailable")
        }
    }
    impl Engine for FailingEngine {
        fn speak(&mut self, _: &str) {
            unreachable!()
        }
        fn set_volume(&mut self, _: u8) {
            unreachable!()
        }
        fn set_rate(&mut self, _: i8) {
            unreachable!()
        }
    }

    #[test]
    fn failed_init_disables_tts_for_good() {
        static TTS: Tts<FailingEngine> = Tts::new(FailingEngine::init);
        // The first call only spawns the worker; it can't know yet that init will fail.
        TTS.speak_alert("lost");
        assert!(wait_for(|| TTS.worker_dead.load(Ordering::Acquire)));
        assert!(!TTS.ensure_worker());
        {
            let slot = TTS.lock_worker();
            assert!(slot.thread.is_none());
            assert!(slot.thread_failed);
        }
        TTS.set_voice_settings(10, 1);
        TTS.speak_alert("ignored");
        assert_eq!(TTS.queue.lock().len(), 1);
        // No worker left, so shutdown is a no-op.
        TTS.shutdown();
        assert_eq!(TTS.queue.lock().len(), 1);
    }

    static SLOW_SPOKEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    struct SlowEngine;
    impl Engine for SlowEngine {
        fn speak(&mut self, text: &str) {
            SLOW_SPOKEN.lock().unwrap().push(text.to_owned());
            std::thread::sleep(Duration::from_millis(200));
        }
        fn set_volume(&mut self, _: u8) {}
        fn set_rate(&mut self, _: i8) {}
    }

    #[test]
    fn shutdown_drops_still_queued_commands() {
        static TTS: Tts<SlowEngine> = Tts::new(|| Ok(SlowEngine));
        TTS.speak_alert("a");
        assert!(wait_for(|| SLOW_SPOKEN.lock().unwrap().len() == 1));
        TTS.speak_alert("b");
        TTS.speak_alert("c");
        TTS.shutdown();
        assert!(TTS.queue.lock().is_empty());
        assert_eq!(*SLOW_SPOKEN.lock().unwrap(), vec!["a".to_owned()]);
    }

    #[test]
    fn shutdown_without_worker_is_noop() {
        static TTS: Tts<SlowEngine> = Tts::new(|| Ok(SlowEngine));
        TTS.shutdown();
        assert!(!TTS.should_exit.load(Ordering::Acquire));
    }
}
