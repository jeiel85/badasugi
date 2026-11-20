fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // GPU(Vulkan) 빌드: vulkan-1.dll 을 처음 쓸 때 불러오게(/DELAYLOAD) — Vulkan 이 없는 PC 에서도 실행 파일이 뜨고
    // CPU 로 돈다 (asr::gpu). 여기서는 이 패키지의 예제 · 시험용, 앱은 src-tauri/build.rs 가 같은 일을 한다.
    if std::env::var_os("CARGO_FEATURE_VULKAN").is_some() && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:vulkan-1.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }
}
