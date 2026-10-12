//! Windows Core Audio 互操作层。
//!
//! 分成四块：
//! - [`com`]：GUID / HRESULT / RAII 接口指针 / HSTRING / vtable 槽位调用
//! - [`endpoint`]：文档化的端点枚举（`IMMDeviceEnumerator` 系列）
//! - [`policy`]：**未公开**的 `AudioPolicyConfig`，即"按应用改设备"的能力本身
//! - [`session`]：音频会话枚举，仅用于诊断
//!
//! [`AudioEnv`] 把这几块和一个 COM 套间守卫绑在一起，是上层唯一需要接触的入口。

// COM 接口常量刻意保留原始大小写（IID_IMMDeviceEnumerator 这种），
// 改成 Rust 风格的 IID_IMMDEVICE_ENUMERATOR 会让它和系统头文件对不上、检索困难。
#![allow(non_upper_case_globals)]

pub mod com;
pub mod endpoint;
pub mod policy;
pub mod session;

use serde::{Deserialize, Serialize};

use com::{ComError, ComGuard, ComPtr, Guid};
use endpoint::{AudioEndpoint, E_RENDER};
use policy::{AudioPolicyConfig, PersistedRoute};

/// `IAudioSessionManager2`
pub const IID_IAudioSessionManager2: Guid =
    Guid::new(0x77AA_99A0, 0x1BD6, 0x484F, [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F]);
/// `IAudioSessionControl2`
pub const IID_IAudioSessionControl2: Guid =
    Guid::new(0xBFB7_FF88, 0x7239, 0x4FC9, [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D]);

// ------------------------------------------------------------------ 角色 ----

/// 音频角色。系统把同一个应用的音频按用途分成三路：
/// 控制音（`eConsole`）、媒体（`eMultimedia`）、通话音（`eCommunications`）。
///
/// 默认只接管前两路——不去动通话音，免得把语音通话从耳机里拽走。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

impl Role {
    pub const fn as_i32(self) -> i32 {
        match self {
            Role::Console => endpoint::ROLE_CONSOLE,
            Role::Multimedia => endpoint::ROLE_MULTIMEDIA,
            Role::Communications => endpoint::ROLE_COMMUNICATIONS,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Role::Console => "控制音",
            Role::Multimedia => "媒体",
            Role::Communications => "通话音",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "console" | "控制音" => Some(Role::Console),
            "multimedia" | "媒体" => Some(Role::Multimedia),
            "communications" | "通话音" => Some(Role::Communications),
            _ => None,
        }
    }

    /// 默认接管的两路。
    pub fn default_set() -> Vec<Role> {
        vec![Role::Console, Role::Multimedia]
    }
}

// -------------------------------------------------------------- 写入结果 ----

#[derive(Debug, Default, Clone)]
pub struct RouteOutcome {
    pub roles_attempted: usize,
    pub roles_written: usize,
    /// 出现了 `PROCESS_NO_AUDIO`：目标进程还没有音频会话，系统没接受写入。
    pub no_audio_session: bool,
    /// 写完之后读回校验的结果。`None` 表示没做成（一个 role 都没写进去）。
    pub verified: Option<bool>,
    pub errors: Vec<String>,
}

impl RouteOutcome {
    /// 是否真的写进去了至少一路。
    pub fn applied(&self) -> bool {
        self.roles_written > 0
    }

    pub fn summary(&self) -> String {
        if !self.errors.is_empty() && self.roles_written == 0 {
            return format!("写入失败：{}", self.errors.join("；"));
        }
        if self.no_audio_session && self.roles_written == 0 {
            return "暂未生效（该进程还没有音频会话，稍后重试）".into();
        }
        format!(
            "已写入 {}/{} 路，读回校验{}",
            self.roles_written,
            self.roles_attempted,
            match self.verified {
                Some(true) => "通过",
                Some(false) => "不一致",
                None => "未做",
            }
        )
    }
}

// ------------------------------------------------------------- 环境入口 ----

/// COM 环境 + 端点枚举器 + 未公开策略接口的组合体。
///
/// 创建时会为当前线程初始化 COM；析构时按需反初始化。所有方法都必须在创建它的那个线程上调用。
pub struct AudioEnv {
    guard: ComGuard,
    enumerator: ComPtr,
    policy: AudioPolicyConfig,
}

impl AudioEnv {
    /// 初始化（多线程套间）。命令行工具用这个。
    pub fn init() -> Result<Self, ComError> {
        Self::init_in(com::Apartment::Multithreaded)
    }

    /// 按指定套间模型初始化。
    ///
    /// 会开窗的进程必须用 [`com::Apartment::SingleThreaded`]：winit 建窗时调 `OleInitialize`，
    /// 而它在本线程已是 MTA 的情况下会 panic。
    pub fn init_in(apartment: com::Apartment) -> Result<Self, ComError> {
        let guard = ComGuard::new(apartment);
        if !guard.usable() {
            return Err(ComError::new("CoInitializeEx", guard.hresult()));
        }
        let enumerator = endpoint::device_enumerator()?;
        let policy = AudioPolicyConfig::create()?;
        Ok(Self { guard, enumerator, policy })
    }

    pub fn com_status(&self) -> String {
        format!("{} —— {}", self.guard.apartment().label(), self.guard.describe())
    }

    pub fn policy(&self) -> &AudioPolicyConfig {
        &self.policy
    }

    /// 当前所有活跃渲染端点。
    pub fn endpoints(&self) -> Result<Vec<AudioEndpoint>, ComError> {
        endpoint::list_render_endpoints(&self.enumerator)
    }

    /// 系统默认渲染设备的原始 ID。
    pub fn default_endpoint_id(&self) -> Option<String> {
        endpoint::default_render_endpoint(&self.enumerator)
    }

    /// 当前有音频会话的进程（跨全部端点合并）。诊断用。
    pub fn session_pids(&self) -> Vec<u32> {
        session::session_pids(&self.enumerator)
    }

    /// 读某个进程在指定角色上的当前路由。
    pub fn get_route(&self, pid: u32, role: Role) -> Result<PersistedRoute, ComError> {
        self.policy.get_persisted(pid, E_RENDER, role.as_i32())
    }

    /// 把某个进程恢复成"跟随系统默认"。
    ///
    /// 走的是"写入空字符串"这条未文档化但实测可用的路径（详见
    /// [`policy::AudioPolicyConfig::clear_persisted`]）。失败的进程记账保留，下轮重试。
    pub fn clear_route(&self, pid: u32, roles: &[Role]) -> RouteOutcome {
        let mut outcome = RouteOutcome::default();

        for &role in roles {
            outcome.roles_attempted += 1;
            match self.policy.clear_persisted(pid, E_RENDER, role.as_i32()) {
                Ok(true) => outcome.roles_written += 1,
                Ok(false) => outcome.no_audio_session = true,
                Err(e) => outcome.errors.push(format!("{}: {e}", role.label())),
            }
        }

        if outcome.roles_written > 0 {
            if let Some(&first) = roles.first() {
                // 这里的"校验通过"= 读回来不再是任何具体设备
                outcome.verified = match self.get_route(pid, first) {
                    Ok(route) => Some(route.device_id().is_none()),
                    Err(_) => None,
                };
            }
        }
        outcome
    }

    /// 把某个进程固定到指定端点，并读回校验。
    pub fn route(&self, pid: u32, endpoint: &AudioEndpoint, roles: &[Role]) -> RouteOutcome {
        self.route_to_id(pid, &endpoint.id, roles)
    }

    /// 同上，但直接给原始设备 ID。还原备份时用。
    pub fn route_to_id(&self, pid: u32, raw_endpoint_id: &str, roles: &[Role]) -> RouteOutcome {
        let swd = endpoint::to_swd(raw_endpoint_id);
        let mut outcome = RouteOutcome::default();

        for &role in roles {
            outcome.roles_attempted += 1;
            match self.policy.set_persisted(pid, E_RENDER, role.as_i32(), &swd) {
                Ok(true) => outcome.roles_written += 1,
                Ok(false) => outcome.no_audio_session = true,
                Err(e) => outcome.errors.push(format!("{}: {e}", role.label())),
            }
        }

        if outcome.roles_written > 0 {
            if let Some(&first) = roles.first() {
                outcome.verified = match self.get_route(pid, first) {
                    Ok(route) => Some(route.device_id() == Some(raw_endpoint_id)),
                    Err(_) => None,
                };
            }
        }
        outcome
    }
}
