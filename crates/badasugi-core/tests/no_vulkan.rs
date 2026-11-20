//! Vulkan 로더(vulkan-1.dll)가 없는 PC 흉내: DLL 을 실행 파일 폴더에서만 찾게 해 시스템의 vulkan-1.dll 을 가린 뒤
//! ggml 장치를 찾는다 — 앱이 죽지 않고 GPU 없이(CPU 만) 열려야 한다. (GPU 빌드에서만: `--features vulkan`)
//! 다른 시험과 DLL 검색 설정이 섞이지 않게 따로 된 시험 실행 파일로 둔다.
#![cfg(all(windows, feature = "vulkan"))]

#[link(name = "kernel32")]
extern "system" {
    fn SetDefaultDllDirectories(flags: u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
}

const LOAD_LIBRARY_SEARCH_APPLICATION_DIR: u32 = 0x0000_0200;

#[test]
fn missing_vulkan_loader_falls_back_to_cpu() {
    let name: Vec<u16> = "vulkan-1.dll\0".encode_utf16().collect();
    assert!(unsafe { GetModuleHandleW(name.as_ptr()) }.is_null(), "아직 vulkan-1.dll 을 불러오지 않았어야 한다 (지연 로드)");
    assert_ne!(unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_APPLICATION_DIR) }, 0);
    // 처음 ggml 장치를 찾을 때 Vulkan 백엔드가 vulkan-1.dll 을 찾다 실패 → 실패 훅의 가짜 함수 → 예외를 잡아 GPU 없음
    assert_eq!(badasugi_core::asr::gpu::find(), None);
    assert!(badasugi_core::asr::gpu::loader_missing(), "실패 훅을 거쳤어야 한다");
}
