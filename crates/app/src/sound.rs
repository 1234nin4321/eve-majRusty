//! WAV/MP3 sound alerts: Media Foundation decodes to PCM, winmm plays it.
//!
//! The path check, volume packing and the worker queue are platform-neutral; the decoder and
//! player behind them are Windows-only.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use eve_maj_core::log::Scope;

const SLOG: Scope = Scope::new("sound");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundError {
    NonLocalSoundPath,
    MFStartupFailed,
    CreateSourceReaderFailed,
    CreateMediaTypeFailed,
    SetMediaTypeFailed,
    SetCurrentMediaTypeFailed,
    GetCurrentMediaTypeFailed,
    UnknownAudioFormat,
    ReadSampleFailed,
    ConvertBufferFailed,
    LockBufferFailed,
    CreateEventFailed,
    WaveOutOpenFailed,
    WaveOutPrepareFailed,
    WaveOutWriteFailed,
}

impl std::fmt::Display for SoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error.{self:?}")
    }
}

impl std::error::Error for SoundError {}

/// Media Foundation will open URLs and network shares too; a sound path from an imported profile pointing at \\\\host\\share would leak the user's NTLM credentials to that host, so only local files are played.
pub fn is_local_sound_path(path: &str) -> bool {
    let b = path.as_bytes();
    if b.len() < 3 {
        return false;
    }
    if path.contains("://") {
        return false;
    }
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    !(is_sep(b[0]) && is_sep(b[1]))
}

/// waveOutSetVolume's packed left/right level for a 0-100 percentage (values above 100 clamp).
pub fn pack_volume(volume_percent: u8) -> u32 {
    let clamped = volume_percent.min(100);
    let level = u32::from(clamped) * 0xFFFF / 100;
    (level << 16) | level
}

// Mirrors tts.zig's lazy worker/queue skeleton, so overlapping alerts play in full instead of cutting each other off.
struct Command {
    path: String,
    volume_percent: u8,
}

/// The per-thread setup, teardown and blocking player the worker runs; swapped out in tests.
pub struct Backend {
    /// Runs once on the worker thread before any sound plays; false marks the worker dead.
    pub startup: fn() -> bool,
    /// Runs on the worker thread as it exits, only after a successful `startup`.
    pub shutdown: fn(),
    pub play: fn(&str, u8) -> Result<(), SoundError>,
}

#[derive(Default)]
struct WorkerSlot {
    thread: Option<JoinHandle<()>>,
    thread_failed: bool,
}

/// FIFO alert queue served by one lazily-started worker thread.
pub struct AlertQueue {
    backend: Backend,
    queue: Mutex<VecDeque<Command>>,
    slot: Mutex<WorkerSlot>,
    should_exit: AtomicBool,
    worker_dead: AtomicBool,
}

const WORKER_POLL_MS: u64 = 50;

impl AlertQueue {
    pub const fn new(backend: Backend) -> Self {
        Self {
            backend,
            queue: Mutex::new(VecDeque::new()),
            slot: Mutex::new(WorkerSlot { thread: None, thread_failed: false }),
            should_exit: AtomicBool::new(false),
            worker_dead: AtomicBool::new(false),
        }
    }

    fn push(&self, cmd: Command) -> Result<(), String> {
        let mut items = self.queue.lock().map_err(|e| e.to_string())?;
        items.push_back(cmd);
        Ok(())
    }

    fn pop(&self) -> Option<Command> {
        let mut items = match self.queue.lock() {
            Ok(items) => items,
            Err(err) => {
                SLOG.warn(format_args!("Failed to lock command queue mutex: {err}"));
                return None;
            }
        };
        items.pop_front()
    }

    fn worker_main(&self) {
        if !(self.backend.startup)() {
            SLOG.warn(format_args!("Sound worker unavailable (MFStartup failed)"));
            self.worker_dead.store(true, Ordering::Release);
            return;
        }
        SLOG.info(format_args!("Sound worker initialized"));

        while !self.should_exit.load(Ordering::Acquire) {
            let Some(cmd) = self.pop() else {
                std::thread::sleep(Duration::from_millis(WORKER_POLL_MS));
                continue;
            };
            if let Err(err) = (self.backend.play)(&cmd.path, cmd.volume_percent) {
                SLOG.warn(format_args!("Failed to play sound alert '{}': {}", cmd.path, err));
            }
        }
        (self.backend.shutdown)();
    }

    fn ensure_worker(&'static self) -> bool {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
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

        match std::thread::Builder::new().name("sound".into()).spawn(move || self.worker_main()) {
            Ok(thread) => {
                slot.thread = Some(thread);
                true
            }
            Err(err) => {
                SLOG.warn(format_args!("Failed to start sound worker thread: {err}"));
                slot.thread_failed = true;
                false
            }
        }
    }

    /// Queue a sound alert; returns immediately and plays in full FIFO order on the lazily-started worker thread.
    pub fn play_alert(&'static self, path: &str, volume_percent: u8) {
        if !is_local_sound_path(path) {
            SLOG.warn(format_args!("Refusing to play non-local sound path: {path}"));
            return;
        }
        if !self.ensure_worker() {
            return;
        }
        if let Err(err) = self.push(Command { path: path.to_owned(), volume_percent }) {
            SLOG.warn(format_args!("Failed to queue sound alert: {err}"));
        }
    }

    /// Stop the worker thread, if one was ever started. Call once during app shutdown.
    pub fn shutdown(&self) {
        let Some(thread) = self.slot.lock().unwrap_or_else(PoisonError::into_inner).thread.take() else {
            return;
        };
        self.should_exit.store(true, Ordering::Release);
        let _ = thread.join();

        while self.pop().is_some() {}
    }
}

#[cfg(windows)]
pub use imp::{play_alert, play_blocking, shutdown};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use eve_maj_win::sys::core::{GUID, HRESULT};
    use eve_maj_win::sys::Win32::Foundation::CloseHandle;
    use eve_maj_win::sys::Win32::Media::Audio::{
        waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutSetVolume, waveOutUnprepareHeader, waveOutWrite, CALLBACK_EVENT, HWAVEOUT,
        WAVEFORMATEX, WAVEHDR, WAVE_MAPPER,
    };
    use eve_maj_win::sys::Win32::System::Threading::{CreateEventA, ResetEvent, WaitForSingleObject, INFINITE};
    use eve_maj_win::wide;

    use super::{is_local_sound_path, pack_volume, AlertQueue, Backend, SoundError, SLOG};

    // Vtables below are transcribed from mingw-w64's mfobjects.h/mfreadwrite.h - Microsoft's own Learn
    // docs list interface methods alphabetically, not in ABI order, and would silently break these calls.

    const MF_MT_MAJOR_TYPE: GUID = GUID::from_u128(0x48eba18e_f8c9_4687_bf11_0a74c9f96a8f);
    const MF_MT_SUBTYPE: GUID = GUID::from_u128(0xf7e34c9a_42e8_4714_b74b_cb29d72c35e5);
    const MF_MEDIA_TYPE_AUDIO: GUID = GUID::from_u128(0x73647561_0000_0010_8000_00aa00389b71);
    const MF_AUDIO_FORMAT_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
    const MF_MT_AUDIO_NUM_CHANNELS: GUID = GUID::from_u128(0x37e48bf5_645e_4c5b_89de_ada9e29b696a);
    const MF_MT_AUDIO_SAMPLES_PER_SECOND: GUID = GUID::from_u128(0x5faeeae7_0290_4c31_9e8a_c534f68d9dba);
    const MF_MT_AUDIO_BITS_PER_SAMPLE: GUID = GUID::from_u128(0xf2deb57f_40fa_4764_aa33_ed4f2d1ff669);

    const MF_VERSION: u32 = (0x0002 << 16) | 0x0070;
    const MFSTARTUP_LITE: u32 = 0x1;
    const MF_SOURCE_READER_FIRST_AUDIO_STREAM: u32 = 0xfffffffd;
    const MF_SOURCE_READERF_ENDOFSTREAM: u32 = 0x2;

    #[link(name = "mfplat")]
    extern "system" {
        fn MFStartup(version: u32, flags: u32) -> HRESULT;
        fn MFShutdown() -> HRESULT;
        fn MFCreateMediaType(pp_type: *mut *mut c_void) -> HRESULT;
    }

    #[link(name = "mfreadwrite")]
    extern "system" {
        fn MFCreateSourceReaderFromURL(url: *const u16, attributes: *mut c_void, reader: *mut *mut c_void) -> HRESULT;
    }

    type This = *mut c_void;

    /// IMFMediaType prefix up to IMFAttributes::SetGUID.
    #[repr(C)]
    struct MediaTypeVtbl {
        unknown: [usize; 3],
        // GetItem, GetItemType, CompareItem, Compare
        _pad0: [usize; 4],
        get_uint32: unsafe extern "system" fn(This, *const GUID, *mut u32) -> HRESULT,
        // GetUINT64 .. SetDouble
        _pad1: [usize; 16],
        set_guid: unsafe extern "system" fn(This, *const GUID, *const GUID) -> HRESULT,
    }

    /// IMFMediaBuffer prefix up to Unlock.
    #[repr(C)]
    struct MediaBufferVtbl {
        unknown: [usize; 3],
        lock: unsafe extern "system" fn(This, *mut *mut u8, *mut u32, *mut u32) -> HRESULT,
        unlock: unsafe extern "system" fn(This) -> HRESULT,
    }

    /// IMFSample prefix up to ConvertToContiguousBuffer.
    #[repr(C)]
    struct SampleVtbl {
        unknown: [usize; 3],
        // IMFAttributes base (30 slots); none are called here, only ConvertToContiguousBuffer below is.
        _attributes: [usize; 30],
        // GetSampleFlags .. GetBufferByIndex
        _pad: [usize; 8],
        convert_to_contiguous_buffer: unsafe extern "system" fn(This, *mut *mut c_void) -> HRESULT,
    }

    /// IMFSourceReader prefix up to ReadSample.
    #[repr(C)]
    struct SourceReaderVtbl {
        unknown: [usize; 3],
        // GetStreamSelection, SetStreamSelection, GetNativeMediaType
        _pad0: [usize; 3],
        get_current_media_type: unsafe extern "system" fn(This, u32, *mut *mut c_void) -> HRESULT,
        set_current_media_type: unsafe extern "system" fn(This, u32, *mut u32, This) -> HRESULT,
        _set_current_position: usize,
        read_sample: unsafe extern "system" fn(This, u32, u32, *mut u32, *mut u32, *mut i64, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct UnknownVtbl {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(This) -> u32,
    }

    /// Owned COM pointer, released on drop.
    struct Com(This);

    impl Com {
        /// Wraps an out-param result; None when the call failed or produced no object.
        fn from_out(hr: HRESULT, ptr: This) -> Option<Self> {
            if hr < 0 || ptr.is_null() {
                None
            } else {
                Some(Self(ptr))
            }
        }

        /// # Safety
        /// `T` must be a prefix of this object's real vtable layout.
        unsafe fn vtbl<T>(&self) -> &T {
            &**(self.0 as *const *const T)
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { (self.vtbl::<UnknownVtbl>().release)(self.0) };
        }
    }

    struct DecodedPcm {
        data: Vec<u8>,
        channels: u16,
        samples_per_sec: u32,
        bits_per_sample: u16,
    }

    unsafe fn decode_to_pcm(path_w: &[u16]) -> Result<DecodedPcm, SoundError> {
        let mut out = null_mut();
        let hr = MFCreateSourceReaderFromURL(path_w.as_ptr(), null_mut(), &mut out);
        let reader = Com::from_out(hr, out).ok_or(SoundError::CreateSourceReaderFailed)?;
        let r: &SourceReaderVtbl = reader.vtbl();

        let mut out = null_mut();
        let hr = MFCreateMediaType(&mut out);
        let pcm_type = Com::from_out(hr, out).ok_or(SoundError::CreateMediaTypeFailed)?;
        let t: &MediaTypeVtbl = pcm_type.vtbl();

        if (t.set_guid)(pcm_type.0, &MF_MT_MAJOR_TYPE, &MF_MEDIA_TYPE_AUDIO) < 0 {
            return Err(SoundError::SetMediaTypeFailed);
        }
        if (t.set_guid)(pcm_type.0, &MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_PCM) < 0 {
            return Err(SoundError::SetMediaTypeFailed);
        }

        // Triggers Media Foundation's built-in MP3 decoder transform automatically.
        if (r.set_current_media_type)(reader.0, MF_SOURCE_READER_FIRST_AUDIO_STREAM, null_mut(), pcm_type.0) < 0 {
            return Err(SoundError::SetCurrentMediaTypeFailed);
        }

        let mut out = null_mut();
        let hr = (r.get_current_media_type)(reader.0, MF_SOURCE_READER_FIRST_AUDIO_STREAM, &mut out);
        let actual_type = Com::from_out(hr, out).ok_or(SoundError::GetCurrentMediaTypeFailed)?;
        let a: &MediaTypeVtbl = actual_type.vtbl();

        let mut channels = 0u32;
        let mut samples_per_sec = 0u32;
        let mut bits_per_sample = 0u32;
        (a.get_uint32)(actual_type.0, &MF_MT_AUDIO_NUM_CHANNELS, &mut channels);
        (a.get_uint32)(actual_type.0, &MF_MT_AUDIO_SAMPLES_PER_SECOND, &mut samples_per_sec);
        (a.get_uint32)(actual_type.0, &MF_MT_AUDIO_BITS_PER_SAMPLE, &mut bits_per_sample);
        if channels == 0 || samples_per_sec == 0 || bits_per_sample == 0 {
            return Err(SoundError::UnknownAudioFormat);
        }
        let (Ok(channels), Ok(bits_per_sample)) = (u16::try_from(channels), u16::try_from(bits_per_sample)) else {
            return Err(SoundError::UnknownAudioFormat);
        };

        let mut data = Vec::new();
        loop {
            let mut sample_flags = 0u32;
            let mut out = null_mut();
            let hr = (r.read_sample)(reader.0, MF_SOURCE_READER_FIRST_AUDIO_STREAM, 0, null_mut(), &mut sample_flags, null_mut(), &mut out);
            let sample = (!out.is_null()).then(|| Com(out));
            if hr < 0 {
                return Err(SoundError::ReadSampleFailed);
            }
            if sample_flags & MF_SOURCE_READERF_ENDOFSTREAM != 0 {
                break;
            }
            let Some(sample) = sample else {
                continue;
            };
            let s: &SampleVtbl = sample.vtbl();

            let mut out = null_mut();
            let hr = (s.convert_to_contiguous_buffer)(sample.0, &mut out);
            let buffer = Com::from_out(hr, out).ok_or(SoundError::ConvertBufferFailed)?;
            let b: &MediaBufferVtbl = buffer.vtbl();

            let mut data_ptr: *mut u8 = null_mut();
            let mut current_len = 0u32;
            if (b.lock)(buffer.0, &mut data_ptr, null_mut(), &mut current_len) < 0 || data_ptr.is_null() {
                return Err(SoundError::LockBufferFailed);
            }
            data.extend_from_slice(std::slice::from_raw_parts(data_ptr, current_len as usize));
            (b.unlock)(buffer.0);
        }

        Ok(DecodedPcm { data, channels, samples_per_sec, bits_per_sample })
    }

    /// Calls MFShutdown on drop, pairing a successful MFStartup.
    struct MfSession;

    impl MfSession {
        fn start() -> Option<Self> {
            (unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } >= 0).then_some(Self)
        }
    }

    impl Drop for MfSession {
        fn drop(&mut self) {
            unsafe { MFShutdown() };
        }
    }

    /// Decodes and plays `path` (WAV/MP3), blocking until done; public so config.exe's "Test Sound" button can call it directly, bypassing the worker queue below.
    pub fn play_blocking(path: &str, volume_percent: u8) -> Result<(), SoundError> {
        if !is_local_sound_path(path) {
            SLOG.warn(format_args!("Refusing to play non-local sound path: {path}"));
            return Err(SoundError::NonLocalSoundPath);
        }
        let path_w = wide(path);

        // MF state is per-process, not shared with the main app's process - config.exe's direct callers need this too.
        let _mf = MfSession::start().ok_or(SoundError::MFStartupFailed)?;

        let pcm = unsafe { decode_to_pcm(&path_w) }?;
        unsafe { play_pcm(&pcm, volume_percent) }
    }

    unsafe fn play_pcm(pcm: &DecodedPcm, volume_percent: u8) -> Result<(), SoundError> {
        let bytes_per_sample = pcm.bits_per_sample / 8;
        let format = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM
            nChannels: pcm.channels,
            nSamplesPerSec: pcm.samples_per_sec,
            nAvgBytesPerSec: pcm.samples_per_sec * u32::from(pcm.channels) * u32::from(bytes_per_sample),
            nBlockAlign: pcm.channels * bytes_per_sample,
            wBitsPerSample: pcm.bits_per_sample,
            cbSize: 0,
        };

        let event = CreateEventA(null(), 1, 0, null());
        if event.is_null() {
            return Err(SoundError::CreateEventFailed);
        }
        let result = play_with_event(pcm, &format, event, volume_percent);
        CloseHandle(event);
        result
    }

    unsafe fn play_with_event(pcm: &DecodedPcm, format: &WAVEFORMATEX, event: *mut c_void, volume_percent: u8) -> Result<(), SoundError> {
        let mut hwo: HWAVEOUT = null_mut();
        if waveOutOpen(&mut hwo, WAVE_MAPPER, format, event as usize, 0, CALLBACK_EVENT) != 0 || hwo.is_null() {
            return Err(SoundError::WaveOutOpenFailed);
        }

        // waveOutOpen signals `event` once on its own (WOM_OPEN); clear that before waiting on WOM_DONE.
        ResetEvent(event);
        waveOutSetVolume(hwo, pack_volume(volume_percent));

        let header_size = std::mem::size_of::<WAVEHDR>() as u32;
        let mut header = WAVEHDR {
            lpData: pcm.data.as_ptr().cast_mut(),
            dwBufferLength: pcm.data.len() as u32,
            dwBytesRecorded: 0,
            dwUser: 0,
            dwFlags: 0,
            dwLoops: 0,
            lpNext: null_mut(),
            reserved: 0,
        };
        let result = if waveOutPrepareHeader(hwo, &mut header, header_size) != 0 {
            Err(SoundError::WaveOutPrepareFailed)
        } else {
            let result = if waveOutWrite(hwo, &mut header, header_size) != 0 {
                Err(SoundError::WaveOutWriteFailed)
            } else {
                WaitForSingleObject(event, INFINITE);
                Ok(())
            };
            waveOutUnprepareHeader(hwo, &mut header, header_size);
            result
        };
        waveOutClose(hwo);
        result
    }

    // The worker holds its own MF session for its whole lifetime, as workerMain does in the Zig code.
    static WORKER_MF: std::sync::Mutex<Option<MfSession>> = std::sync::Mutex::new(None);

    fn worker_startup() -> bool {
        let Some(session) = MfSession::start() else {
            return false;
        };
        *WORKER_MF.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
        true
    }

    fn worker_shutdown() {
        WORKER_MF.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    }

    static QUEUE: AlertQueue = AlertQueue::new(Backend { startup: worker_startup, shutdown: worker_shutdown, play: play_blocking });

    /// Queue a sound alert; returns immediately and plays in full FIFO order on the lazily-started worker thread.
    pub fn play_alert(path: &str, volume_percent: u8) {
        QUEUE.play_alert(path, volume_percent);
    }

    /// Stop the worker thread, if one was ever started. Call once during app shutdown.
    pub fn shutdown() {
        QUEUE.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn local_paths() {
        assert!(is_local_sound_path("C:\\sounds\\alert.wav"));
        assert!(is_local_sound_path("alert.mp3"));
        assert!(is_local_sound_path("./a"));
        assert!(!is_local_sound_path("ab"));
        assert!(!is_local_sound_path(""));
        assert!(!is_local_sound_path("\\\\host\\share\\a.wav"));
        assert!(!is_local_sound_path("//host/share/a.wav"));
        assert!(!is_local_sound_path("\\/host/a.wav"));
        assert!(!is_local_sound_path("http://example.com/a.mp3"));
        assert!(!is_local_sound_path("C:\\x://y"));
    }

    #[test]
    fn volume_packing() {
        assert_eq!(pack_volume(0), 0);
        assert_eq!(pack_volume(100), 0xFFFF_FFFF);
        assert_eq!(pack_volume(255), 0xFFFF_FFFF);
        assert_eq!(pack_volume(50), (0x7FFF << 16) | 0x7FFF);
    }

    static PLAYED: Mutex<Vec<(String, u8)>> = Mutex::new(Vec::new());
    static SHUTDOWNS: AtomicUsize = AtomicUsize::new(0);

    fn record(path: &str, volume: u8) -> Result<(), SoundError> {
        PLAYED.lock().unwrap().push((path.to_owned(), volume));
        Ok(())
    }

    #[test]
    fn queue_plays_fifo_and_shuts_down() {
        static Q: AlertQueue = AlertQueue::new(Backend {
            startup: || true,
            shutdown: || {
                SHUTDOWNS.fetch_add(1, Ordering::SeqCst);
            },
            play: record,
        });
        Q.play_alert("a.wav", 10);
        Q.play_alert("\\\\host\\share\\x.wav", 10);
        Q.play_alert("b.mp3", 20);
        for _ in 0..200 {
            if PLAYED.lock().unwrap().len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Q.shutdown();
        assert_eq!(*PLAYED.lock().unwrap(), vec![("a.wav".to_owned(), 10), ("b.mp3".to_owned(), 20)]);
        assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 1);
        assert!(Q.slot.lock().unwrap().thread.is_none());
    }

    #[test]
    fn dead_worker_stops_queueing() {
        static Q: AlertQueue = AlertQueue::new(Backend {
            startup: || false,
            shutdown: || panic!("shutdown without startup"),
            play: |_, _| panic!("played on a dead worker"),
        });
        assert!(Q.ensure_worker());
        while !Q.worker_dead.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!Q.ensure_worker());
        assert!(Q.slot.lock().unwrap().thread_failed);
        Q.play_alert("a.wav", 50);
        assert!(Q.queue.lock().unwrap().is_empty());
        Q.shutdown();
    }
}
