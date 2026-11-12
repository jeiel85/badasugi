//! Windows 전력 제한(EcoQoS) 해제.
//!
//! Windows 11 은 창이 가려졌거나 트레이에만 있는 프로세스를 "효율 모드"로 보고 저전력 코어·낮은 클럭으로 돌린다.
//! 이 상태에서 whisper.cpp 처럼 여러 스레드가 동기화하며 계산하는 작업은 수십 배 느려지므로,
//! 녹음 중에는 이 프로세스의 실행 속도 제한을 끈다.

/// 프로세스 전체의 실행 속도 제한을 끄고 우선순위를 살짝 올린다.
pub fn disable_power_throttling() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
            ABOVE_NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        let ok = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        if ok == 0 {
            log::warn!("전력 제한 해제 실패");
        }
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

/// 대기 상태로 돌아갈 때: 다시 Windows 가 알아서 절전하도록 맡긴다.
pub fn restore_power_throttling() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
            NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_STATE,
        };
        // ControlMask 0 = 시스템 기본 정책
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: 0,
            StateMask: 0,
        };
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
    }
}
