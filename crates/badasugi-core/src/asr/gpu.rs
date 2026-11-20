//! GPU 가속 (Vulkan 빌드) — 시작할 때 장치를 확인하고, 없거나 열지 못하면 CPU 로.
//!
//! 배포본은 whisper.cpp 를 Vulkan 과 함께 빌드한다(`--features vulkan`). GPU 가 있으면 저절로 쓰고, 설정에서 끌 수 있다.
//!
//! Vulkan 로더(`vulkan-1.dll`)는 그래픽 드라이버가 깔아 주므로 드라이버가 없는 PC(가상 머신 · 서버 · 기본 디스플레이
//! 드라이버)에는 없다. 실행 파일이 이것을 처음부터 불러오면 그런 PC 에서는 앱이 아예 뜨지 않으므로(API 엔진도 못 씀),
//! 처음 쓸 때 불러오게(`/DELAYLOAD`) 빌드하고, 그래도 못 찾으면 아래 실패 훅이 "Vulkan 을 열 수 없음" 만 돌려주는
//! 가짜 `vkGetInstanceProcAddr` 를 건넨다 → ggml 이 Vulkan 을 빼고(예외를 잡아 장치 없음으로) CPU 로만 연다.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(true);
/// GPU 로 열거나 돌리다 실패했다 — 이번 실행 동안은 CPU 로
static BROKEN: AtomicBool = AtomicBool::new(false);
/// GPU 장치를 찾지 못했다 (GPU · 드라이버 · Vulkan 없음) — 이번 실행 동안 다시 찾지 않는다
static NO_DEVICE: AtomicBool = AtomicBool::new(false);

pub fn mark_broken() {
    BROKEN.store(true, Ordering::SeqCst);
}

pub fn is_broken() -> bool {
    BROKEN.load(Ordering::SeqCst)
}

pub fn mark_no_device() {
    NO_DEVICE.store(true, Ordering::SeqCst);
}

/// 이번에 모델을 열 때 GPU 를 해 볼지 — 원하고, 실패한 적 없고, 장치가 없다고 확인되지 않았을 때.
/// 불러온 모델을 다시 쓸지 정하는 열쇠이기도 해서, GPU 가 없는 PC 에서도 CPU 모델을 매번 다시 읽지 않는다.
pub fn should_try() -> bool {
    wanted() && !is_broken() && !NO_DEVICE.load(Ordering::SeqCst)
}

/// 설정 › 녹음 › GPU 가속 — 다음에 모델을 불러올 때부터
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

/// GPU 를 쓸지 (설정 · 환경 변수 `BADASUGI_GPU=0` 이면 끔). GPU 없이 빌드했으면 늘 false
pub fn wanted() -> bool {
    cfg!(feature = "vulkan") && ENABLED.load(Ordering::SeqCst) && std::env::var("BADASUGI_GPU").map_or(true, |v| v != "0")
}

/// 이 빌드가 GPU 를 쓸 수 있게 만들어졌는지
pub const BUILT_WITH_GPU: bool = cfg!(feature = "vulkan");

/// Vulkan 로더가 없어 가짜 함수로 막았는지 (진단 로그용)
pub fn loader_missing() -> bool {
    #[cfg(all(windows, feature = "vulkan"))]
    {
        delayload::MISSING.load(Ordering::SeqCst)
    }
    #[cfg(not(all(windows, feature = "vulkan")))]
    {
        false
    }
}

/// whisper.cpp 가 고를 GPU (ggml 장치 목록에서 GPU · 내장 GPU 중 첫째) → 장치 설명. 없으면 None.
/// 처음 부르면 ggml 이 장치를 찾는다(Vulkan 인스턴스를 만든다) — 수십~수백 ms.
pub fn find() -> Option<String> {
    #[cfg(feature = "vulkan")]
    unsafe {
        use whisper_rs::whisper_rs_sys as sys;
        for i in 0..sys::ggml_backend_dev_count() {
            let d = sys::ggml_backend_dev_get(i);
            let t = sys::ggml_backend_dev_type(d);
            if t == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                || t == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                let s = |p: *const std::os::raw::c_char| {
                    if p.is_null() {
                        String::new()
                    } else {
                        std::ffi::CStr::from_ptr(p).to_string_lossy().trim().to_string()
                    }
                };
                let (name, desc) = (s(sys::ggml_backend_dev_name(d)), s(sys::ggml_backend_dev_description(d)));
                return Some(if desc.is_empty() { name } else { format!("{desc} ({name})") });
            }
        }
    }
    None
}

/// `vulkan-1.dll` 을 못 찾았을 때 (MSVC 지연 로드 실패 훅)
#[cfg(all(windows, feature = "vulkan"))]
pub(crate) mod delayload {
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) static MISSING: AtomicBool = AtomicBool::new(false);

    /// delayimp.h `DelayLoadProc`
    #[repr(C)]
    struct DelayLoadProc {
        import_by_name: i32,
        name_or_ordinal: *const c_char,
    }

    /// delayimp.h `DelayLoadInfo`
    #[repr(C)]
    pub struct DelayLoadInfo {
        cb: u32,
        pidd: *const c_void,
        ppfn: *mut *mut c_void,
        sz_dll: *const c_char,
        dlp: DelayLoadProc,
        hmod_cur: *mut c_void,
        pfn_cur: *mut c_void,
        last_error: u32,
    }

    const DLI_FAIL_LOAD_LIB: u32 = 3;
    const DLI_FAIL_GET_PROC: u32 = 4;
    /// VK_ERROR_INITIALIZATION_FAILED
    const VK_INIT_FAILED: i32 = -3;

    type Hook = unsafe extern "system" fn(u32, *const DelayLoadInfo) -> *mut c_void;

    /// 지연 로드 도우미(delayimp.lib)가 실패할 때 부르는 훅 — 우리가 정의하면 기본값(없음) 대신 이것을 쓴다
    #[no_mangle]
    #[used]
    pub static __pfnDliFailureHook2: Option<Hook> = Some(on_failure);

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }

    unsafe fn is_vulkan(dll: *const c_char) -> bool {
        !dll.is_null() && unsafe { CStr::from_ptr(dll) }.to_bytes().eq_ignore_ascii_case(b"vulkan-1.dll")
    }

    unsafe extern "system" fn on_failure(notify: u32, info: *const DelayLoadInfo) -> *mut c_void {
        let Some(info) = (unsafe { info.as_ref() }) else {
            return std::ptr::null_mut();
        };
        if !unsafe { is_vulkan(info.sz_dll) } {
            return std::ptr::null_mut();
        }
        match notify {
            // 로더가 없다 → 아무 모듈(이 실행 파일)이나 돌려주면 도우미가 거기서 함수를 찾다 실패해 아래로 온다
            DLI_FAIL_LOAD_LIB => {
                MISSING.store(true, Ordering::SeqCst);
                unsafe { GetModuleHandleW(std::ptr::null()) }
            }
            DLI_FAIL_GET_PROC => {
                MISSING.store(true, Ordering::SeqCst);
                let by_name = info.dlp.import_by_name != 0 && !info.dlp.name_or_ordinal.is_null();
                if by_name && unsafe { CStr::from_ptr(info.dlp.name_or_ordinal) }.to_bytes() == b"vkGetInstanceProcAddr" {
                    get_instance_proc_addr as *mut c_void
                } else {
                    fail as *mut c_void
                }
            }
            _ => std::ptr::null_mut(),
        }
    }

    /// 가짜 `vkGetInstanceProcAddr` — 버전 묻기만 "초기화 실패" 로 답하고 나머지는 없음
    pub(crate) unsafe extern "system" fn get_instance_proc_addr(_instance: *mut c_void, name: *const c_char) -> *mut c_void {
        if !name.is_null() && unsafe { CStr::from_ptr(name) }.to_bytes() == b"vkEnumerateInstanceVersion" {
            enumerate_instance_version as *mut c_void
        } else {
            std::ptr::null_mut()
        }
    }

    /// 가짜 `vkEnumerateInstanceVersion` — ggml 은 이것이 실패하면 예외를 잡아 Vulkan 장치 없이 간다
    pub(crate) unsafe extern "system" fn enumerate_instance_version(_version: *mut u32) -> i32 {
        VK_INIT_FAILED
    }

    /// 그 밖의 Vulkan 함수 (부를 일이 없지만 혹시 불리면 실패만)
    unsafe extern "system" fn fail() -> i32 {
        VK_INIT_FAILED
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn missing_loader_gets_a_stub_that_refuses_vulkan() {
            let dll = c"vulkan-1.dll";
            let proc_name = c"vkGetInstanceProcAddr";
            let info = DelayLoadInfo {
                cb: std::mem::size_of::<DelayLoadInfo>() as u32,
                pidd: std::ptr::null(),
                ppfn: std::ptr::null_mut(),
                sz_dll: dll.as_ptr(),
                dlp: DelayLoadProc { import_by_name: 1, name_or_ordinal: proc_name.as_ptr() },
                hmod_cur: std::ptr::null_mut(),
                pfn_cur: std::ptr::null_mut(),
                last_error: 0,
            };
            unsafe {
                assert!(!on_failure(DLI_FAIL_LOAD_LIB, &info).is_null(), "모듈 대신 이 실행 파일");
                let gipa = on_failure(DLI_FAIL_GET_PROC, &info);
                assert_eq!(gipa, get_instance_proc_addr as *mut c_void);
                let ver = get_instance_proc_addr(std::ptr::null_mut(), c"vkEnumerateInstanceVersion".as_ptr());
                assert_eq!(ver, enumerate_instance_version as *mut c_void);
                assert!(get_instance_proc_addr(std::ptr::null_mut(), c"vkCreateInstance".as_ptr()).is_null());
                let mut v = 0u32;
                assert_eq!(enumerate_instance_version(&mut v), VK_INIT_FAILED);
                // 다른 DLL 의 실패는 건드리지 않는다
                let other = DelayLoadInfo { sz_dll: c"other.dll".as_ptr(), ..info };
                assert!(on_failure(DLI_FAIL_LOAD_LIB, &other).is_null());
            }
            assert!(MISSING.load(Ordering::SeqCst));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_device_stops_trying_the_gpu_for_this_run() {
        set_enabled(true);
        assert_eq!(should_try(), BUILT_WITH_GPU && std::env::var("BADASUGI_GPU").map_or(true, |v| v != "0"));
        mark_no_device();
        assert!(!should_try(), "장치가 없다고 확인되면 다시 찾지 않는다 (CPU 모델을 그대로 다시 씀)");
        assert!(!is_broken(), "장치가 없는 것은 실패가 아니다 (스모크 점검)");
    }
}
