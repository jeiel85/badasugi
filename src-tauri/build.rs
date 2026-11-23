fn main() {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    // GPU(Vulkan) 빌드: vulkan-1.dll 을 처음 쓸 때 불러오게(/DELAYLOAD) — 그래픽 드라이버가 없어 Vulkan 이 없는 PC 에서도
    // 앱이 뜨고 CPU 로 받아 적는다 (badasugi_core::asr::gpu)
    if std::env::var_os("CARGO_FEATURE_VULKAN").is_some() && windows {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:vulkan-1.dll");
    }
    // 바로 가기로 다시 실행하면 이 실행 파일은 앞선 앱에 명령만 넘기고 끝난다(src/forward.rs). 넘기는 데 필요 없는 DLL 은
    // 처음 쓸 때 불러오게 해서 그 짧은 실행이 DLL 수십 개를 올리지 않게 한다 (앱으로 뜰 때는 쓰는 순간 불려 온다)
    if windows {
        for dll in DELAY_LOADED {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-lib=delayimp");
    }
    // 두 번째 실행을 tauri 를 띄우기 전에 앞선 앱으로 넘기려면(src/forward.rs) 단일 실행 창 이름의 바탕인 앱 식별자가 필요하다.
    // tauri 와 같이 tauri.conf.json 에서 읽고, 빌드할 때 TAURI_CONFIG 로 덮어쓴 값이 있으면 그것을 쓴다
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-env-changed=TAURI_CONFIG");
    let conf: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("tauri.conf.json").expect("tauri.conf.json")).expect("tauri.conf.json");
    let over = std::env::var("TAURI_CONFIG").ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let identifier = over
        .as_ref()
        .and_then(|o| o["identifier"].as_str())
        .or_else(|| conf["identifier"].as_str())
        .expect("tauri.conf.json identifier");
    println!("cargo:rustc-env=BADASUGI_IDENTIFIER={identifier}");
    tauri_build::build()
}

/// 처음 쓸 때 불러올 DLL — 창 · 셸 · COM(tao · wry · muda · windows 크레이트의 오류 보고), 오디오(cpal),
/// 네트워크 · 인증서(reqwest · native-tls · keyring), ONNX Runtime 의 DirectML · Direct3D · 장치 목록 · 진단.
/// user32 · kernel32 · advapi32 · C/C++ 런타임처럼 어차피 같이 올라오거나 시작 전에 필요한 것은 넣지 않는다
const DELAY_LOADED: &[&str] = &[
    "shell32.dll",
    "ole32.dll",
    "oleaut32.dll",
    "combase.dll",
    "api-ms-win-core-winrt-error-l1-1-0.dll",
    "comctl32.dll",
    "shlwapi.dll",
    "dwmapi.dll",
    "mmdevapi.dll",
    "crypt32.dll",
    "secur32.dll",
    "ws2_32.dll",
    "bcrypt.dll",
    "directml.dll",
    "d3d12.dll",
    "dxgi.dll",
    "setupapi.dll",
    "dbghelp.dll",
];
