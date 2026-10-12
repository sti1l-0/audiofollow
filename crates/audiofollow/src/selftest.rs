//! 无界面自检。把整条链路真实跑一遍并如实打印，不猜、不美化。
//!
//! - `--selftest`：只读。枚举显示器 / 音频设备 / 窗口归属 / 路由计划 / 接口状态。
//! - `--write-test [pid]`：在**单个目标进程**上做一次可回滚的写入往返。
//!   先记录原值，写入 → 读回校验 → 还原，并额外探测"写空值能否恢复跟随默认"。
//!
//! 为什么写入测试要限定单个进程：接口的持久化设置是**系统级**的，改了就留在用户的
//! 音量合成器里。自检必须把影响面控制到最小并且留下还原动作，绝不能顺手跑一遍全量。

use af_core::audio::endpoint::{AudioEndpoint, E_RENDER};
use af_core::audio::policy::PersistedRoute;
use af_core::audio::{AudioEnv, Role};
use af_core::config::AppConfig;
use af_core::plan;
use af_core::win::{self, window};

fn rule(title: &str) {
    println!();
    println!("--- {title} {}", "-".repeat(60usize.saturating_sub(title.len())));
}

fn read_role(env: &AudioEnv, pid: u32, role: Role) -> Result<PersistedRoute, String> {
    env.get_route(pid, role).map_err(|e| e.to_string())
}

pub fn run(write_test: bool, args: &[String]) -> i32 {
    println!("============================================================");
    println!(" AudioFollow 自检");
    println!("============================================================");

    let self_pid = std::process::id();

    // ---------------- [0] 进程 ----------------
    rule("[0] 进程");
    println!("  自身 pid        : {self_pid}");
    println!("  可执行文件      : {}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into()));
    let (config, warn) = AppConfig::load();
    println!("  配置文件        : {}", AppConfig::path().display());
    if let Some(w) = &warn {
        println!("  注意            : {w}");
    }
    println!(
        "  自动跟随        : {}   轮询 {} ms",
        config.auto_follow, config.poll_ms
    );
    println!(
        "  接管的音频角色  : {}",
        config
            .roles
            .iter()
            .map(|r| r.label())
            .collect::<Vec<_>>()
            .join(" / ")
    );

    // ---------------- [1] 显示器 ----------------
    rule("[1] 显示器");
    let monitors = win::monitor::enumerate();
    if monitors.is_empty() {
        println!("  未检测到显示器");
    }
    for (i, m) in monitors.iter().enumerate() {
        println!("  {}", m.label(i));
        println!("      设备名     : {}", m.device);
        println!("      全景矩形   : {:?}", m.rect);
        println!("      工作区     : {:?}", m.work);
        println!("      DPI        : {} ({}%)", m.dpi, m.scale_percent());
    }

    // ---------------- [2] 音频环境 ----------------
    rule("[2] 音频接口");
    let env = match AudioEnv::init() {
        Ok(env) => env,
        Err(e) => {
            println!("  初始化失败：{e}");
            println!("  可能原因：Core Audio 服务未运行，或 AudioPolicyConfig 的 IID 不在白名单里。");
            return 2;
        }
    };
    println!("  CoInitializeEx  : {}", env.com_status());
    println!("  IID（运行时发现）: {}", env.policy().picked_iid());
    println!(
        "  白名单校验      : {}",
        if af_core::audio::policy::KNOWN_VALID_IIDS
            .contains(&env.policy().picked_iid().to_string().as_str())
        {
            "通过"
        } else {
            "未通过"
        }
    );
    println!("  GetIids 返回的全部接口：");
    for (i, g) in env.policy().all_iids().iter().enumerate() {
        let mark = if *g == env.policy().picked_iid() { "  ← 本次选用" } else { "" };
        println!("      [{i}] {g}{mark}");
    }

    // ---------------- [3] 音频设备 ----------------
    rule("[3] 音频输出设备");
    let endpoints = match env.endpoints() {
        Ok(eps) => eps,
        Err(e) => {
            println!("  枚举失败：{e}");
            return 2;
        }
    };
    if endpoints.is_empty() {
        println!("  没有活跃的音频输出设备");
    }
    for (i, ep) in endpoints.iter().enumerate() {
        println!("  {}", ep.label(i));
        println!("      原始 ID    : {}", ep.id);
        println!("      SWD 形式   : {}", ep.swd_id());
    }

    // ---------------- [4] 窗口与归属 ----------------
    rule("[4] 顶层窗口 → 显示器");
    let windows = window::enumerate_app_windows();
    let mut shown = 0usize;
    for w in &windows {
        if w.pid == self_pid {
            continue;
        }
        let mi = window::monitor_index_for(&monitors, w);
        let target = match mi {
            Some(i) => monitors[i].device.clone(),
            None => "<无归属>".to_string(),
        };
        let flag = if w.minimized { " [最小化，不参与判定]" } else { "" };
        println!(
            "  {:>7}  {:<22} {:>12}  {}{}",
            w.pid,
            truncate(&w.exe, 22),
            target,
            truncate(&w.display_name(), 46),
            flag
        );
        shown += 1;
        if shown >= 25 {
            println!("  ...（仅显示前 25 个窗口）");
            break;
        }
    }
    println!("  共 {} 个顶层窗口", windows.len());

    // ---------------- [5] 路由计划 ----------------
    rule("[5] 路由计划（纯计算，尚未写入）");
    let p = plan::build(&config, &monitors, &windows, &endpoints, self_pid);
    if p.is_empty() {
        println!("  计划为空");
    }
    for e in &p.entries {
        println!(
            "  {:<28} → {} / {}",
            e.label(),
            e.monitor_device,
            e.endpoint.name
        );
        println!("      重叠面积   : {} px²", e.overlap);
        if !e.windows.is_empty() {
            println!("      依据窗口   : {}", e.windows.join(" | "));
        }
        if !e.contested_monitors.is_empty() {
            println!(
                "      注意       : 该进程在 {:?} 上也有窗口，已按面积择大",
                e.contested_monitors
            );
        }
    }
    for n in &p.notes {
        println!("  说明：{n}");
    }

    // ---------------- [6] 音频会话（诊断） ----------------
    rule("[6] 当前有音频会话的进程（诊断用，不参与路由决策）");
    let pids = env.session_pids();
    if pids.is_empty() {
        println!("  （没有活跃音频会话——放点声音再跑一次就能看到）");
    }
    for pid in &pids {
        let name = window::process_name(*pid).unwrap_or_else(|| "?".into());
        let sub = match read_role(&env, *pid, Role::Console) {
            Ok(r) => r.describe(),
            Err(e) => format!("读取失败 {e}"),
        };
        println!("  {:>7}  {:<24} 控制音：{}", pid, truncate(&name, 24), sub);
    }

    // ---------------- [7] 可选的写入往返 ----------------
    if write_test {
        rule("[7] 写入往返验证（可回滚）");
        return write_roundtrip(&env, &endpoints, args);
    }

    rule("完成");
    println!("  本次未向系统写入任何内容（只读自检）。");
    println!("  要验证写入路径，运行： audiofollow --write-test");
    0
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        let mut out: String = chars[..n.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

/// 在单个进程上做一次「备份 → 写入 → 读回 → 还原」的往返。
fn write_roundtrip(env: &AudioEnv, endpoints: &[AudioEndpoint], args: &[String]) -> i32 {
    println!("  目标进程的持久化设置会被临时改动，随后立即还原。");
    println!("  注意：还原依赖把「原设备」写回去。若原值本来就是「跟随系统默认」，");
    println!("        该接口没有写回空值的操作，可能留下一条记录（本函数会实测这一点）。");

    if endpoints.is_empty() {
        println!("  没有可用的输出设备，无法测试。");
        return 2;
    }

    // 选目标进程：命令行给了 pid 就用它；否则挑一个有音频会话的普通进程。
    let explicit: Option<u32> = args.iter().find_map(|a| a.parse::<u32>().ok());
    let target = match explicit {
        Some(pid) => pid,
        None => {
            let self_pid = std::process::id();
            match env.session_pids().into_iter().find(|p| *p != self_pid) {
                Some(pid) => pid,
                None => {
                    println!();
                    println!("  找不到有音频会话的进程。请先让某个应用播放声音，再重跑；");
                    println!("  或者显式指定目标： audiofollow --write-test <pid>");
                    return 2;
                }
            }
        }
    };

    let name = window::process_name(target).unwrap_or_else(|| "?".into());
    println!();
    println!("  目标进程：{name} (pid {target})");

    let roles = Role::default_set();

    // 1) 记录原值
    let mut originals: Vec<(Role, PersistedRoute)> = Vec::new();
    for &role in &roles {
        match read_role(env, target, role) {
            Ok(r) => {
                println!("  改动前 {}：{}", role.label(), r.describe());
                originals.push((role, r));
            }
            Err(e) => {
                println!("  改动前 {}：读取失败 {e}", role.label());
                return 2;
            }
        }
    }
    if originals
        .iter()
        .all(|(_, r)| matches!(r, PersistedRoute::NoAudioSession))
    {
        println!();
        println!("  该进程当前没有音频会话，接口会拒绝写入（PROCESS_NO_AUDIO）。");
        println!("  请让这个应用播放声音后重试。");
        return 2;
    }

    // 2) 挑写入目标。
    //
    // 选择原则是「就算最后还原不了，也不要让这个应用的出声行为发生变化」：
    // - 原值本来就是「跟随系统默认」→ 写**系统默认设备**。这样遗留的只是一条冗余记录，
    //   该应用出的声音还是从同一个设备出来。
    // - 原值是某个具体设备 → 还原是精确的（写回原 ID），可以随便挑一个不同的设备来验证。
    let original_is_pinned = originals
        .iter()
        .any(|(_, r)| matches!(r, PersistedRoute::Pinned(_)));
    let current_id = originals
        .iter()
        .find_map(|(_, r)| r.device_id().map(str::to_string));
    let default_id = env.default_endpoint_id();

    let target_ep = if original_is_pinned {
        endpoints
            .iter()
            .find(|e| Some(e.id.as_str()) != current_id.as_deref())
            .unwrap_or(&endpoints[0])
    } else {
        default_id
            .as_deref()
            .and_then(|id| endpoints.iter().find(|e| e.id == id))
            .unwrap_or(&endpoints[0])
    };
    println!("  写入目标：{}", target_ep.name);
    if !original_is_pinned {
        println!(
            "      （目标即当前系统默认设备：即使无法还原，这个应用的出声设备也不会变，\
             只会多一条冗余记录）"
        );
    }

    // 3) 写入
    println!();
    println!("  --- 写入 ---");
    for &role in &roles {
        match env.policy().set_persisted(target, E_RENDER, role.as_i32(), &target_ep.swd_id()) {
            Ok(true) => println!("  {} 写入 -> S_OK（已接受）", role.label()),
            Ok(false) => println!("  {} 写入 -> PROCESS_NO_AUDIO（系统未接受，什么都没改）", role.label()),
            Err(e) => println!("  {} 写入 -> {e}", role.label()),
        }
    }

    // 4) 读回校验
    println!();
    println!("  --- 读回校验 ---");
    let mut all_ok = true;
    for &role in &roles {
        match read_role(env, target, role) {
            Ok(r) => {
                let ok = r.device_id() == Some(target_ep.id.as_str());
                all_ok &= ok;
                println!(
                    "  {} 读回 -> {}  {}",
                    role.label(),
                    r.describe(),
                    if ok { "✅ 与写入一致" } else { "❌ 与写入不一致" }
                );
            }
            Err(e) => {
                all_ok = false;
                println!("  {} 读回失败：{e}", role.label());
            }
        }
    }

    // 5) 还原
    println!();
    println!("  --- 还原 ---");
    for (role, original) in &originals {
        match original {
            PersistedRoute::Pinned(id) => {
                match env.route_to_id(target, id, &[*role]) {
                    o if o.applied() => println!("  {} 写回原设备 {id} ✔", role.label()),
                    o => println!("  {} 写回原设备失败：{}", role.label(), o.summary()),
                }
            }
            other => {
                // 原值不是某个具体设备，那就是"跟随系统默认"。
                // 接口没有文档化的"清除单个进程"，这里实测一下写空字符串行不行。
                println!(
                    "  {} 原值为「{}」，尝试写入空字符串恢复跟随默认：",
                    role.label(),
                    other.describe()
                );
                match env.policy().clear_persisted(target, E_RENDER, role.as_i32()) {
                    Ok(true) => println!(
                        "      -> S_OK  接口接受空值 ✅（结论：可以这样恢复跟随系统默认，\
                         所以本程序退出时能完整还原）"
                    ),
                    Ok(false) => println!("      -> PROCESS_NO_AUDIO  未生效（进程无音频会话）"),
                    Err(e) => println!("      -> {e}  这条路走不通 ❌（会留下一条固定记录）"),
                }
            }
        }
    }

    // 6) 还原后复检
    println!();
    println!("  --- 还原后复检 ---");
    let mut restored = true;
    for (role, original) in &originals {
        match read_role(env, target, *role) {
            Ok(r) => {
                let same = match (original, &r) {
                    (PersistedRoute::Pinned(a), PersistedRoute::Pinned(b)) => a == b,
                    (PersistedRoute::FollowSystemDefault, PersistedRoute::FollowSystemDefault) => true,
                    (PersistedRoute::NoAudioSession, PersistedRoute::NoAudioSession) => true,
                    _ => false,
                };
                if !same {
                    restored = false;
                }
                println!(
                    "  {} 现在 -> {}  {}",
                    role.label(),
                    r.describe(),
                    if same { "✅ 已回到改动前" } else { "⚠️ 与改动前不同" }
                );
            }
            Err(e) => {
                restored = false;
                println!("  {} 读取失败：{e}", role.label());
            }
        }
    }

    println!();
    println!("  ===== 结论 =====");
    println!(
        "  写入路径            : {}",
        if all_ok {
            "可用（写入后读回一致）"
        } else {
            "有疑问（写入与读回不一致，请把上面的输出保留下来）"
        }
    );
    println!(
        "  还原到改动前        : {}",
        if restored { "成功" } else { "未完全成功" }
    );
    println!(
        "  遗留影响            : {}",
        if restored {
            "无".to_string()
        } else {
            format!("该进程的持久化设置可能与原值不同，可在系统「音量合成器」里手动改回。进程 pid {target}")
        }
    );
    if !restored {
        println!();
        println!("  如需彻底重置所有应用的路由（会影响其他程序），谨慎使用：");
        println!("      audiofollow --clear-all --yes");
    }
    0
}

/// 全局清除。会连其它程序设置的路由一起清掉，必须显式确认。
pub fn clear_all(args: &[String]) -> i32 {
    if !args.iter().any(|a| a == "--yes") {
        println!("这会清空**所有**应用的持久化音频路由（包括你在系统音量合成器里手动设过的）。");
        println!("确认请加 --yes：audiofollow --clear-all --yes");
        return 1;
    }
    match AudioEnv::init() {
        Ok(env) => match env.policy().clear_all_persisted() {
            Ok(()) => {
                println!("已清空所有应用的持久化音频路由。");
                0
            }
            Err(e) => {
                println!("清除失败：{e}");
                2
            }
        },
        Err(e) => {
            println!("初始化失败：{e}");
            2
        }
    }
}
