fn main() {
    // GPU(Vulkan) 빌드: vulkan-1.dll 을 처음 쓸 때 불러오게(/DELAYLOAD) — 그래픽 드라이버가 없어 Vulkan 이 없는 PC 에서도
    // 앱이 뜨고 CPU 로 받아 적는다 (badasugi_core::asr::gpu)
    if std::env::var_os("CARGO_FEATURE_VULKAN").is_some() && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:vulkan-1.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }
    tauri_build::build()
}
