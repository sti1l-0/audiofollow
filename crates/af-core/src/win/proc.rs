//! 进程树。
//!
//! # 这个模块解决什么问题
//!
//! Chrome / Edge / Electron / 任何多进程应用的**窗口所属进程**和**音频会话所属进程**
//! 不是同一个：
//!
//! ```text
//! msedge.exe (主进程, pid 1836)   ← 窗口在这里，归属判定用它
//!   ├─ msedge.exe  --type=renderer        ← 页面渲染
//!   ├─ msedge.exe  --type=utility --utility-sub-type=audio.mojom.AudioService
//!   │                                     ← **真正出声的进程**
//!   └─ msedge.exe  --type=gpu-process
//! ```
//!
//! 只按窗口 PID 写路由，这类应用会**完全不生效**：我们改的是主进程的持久化设置，
//! 而主进程根本没有音频会话（写进去还会被 `PROCESS_NO_AUDIO` 挡掉），有会话的是子进程。
//!
//! 做法：拍一张全进程快照拿到 `(pid, ppid)`，构树；把窗口 PID 展开成
//! 「自己 + 全部后代」，逐个写。
//!
//! # 为什么不能无条件相信 `th32ParentProcessID`
//!
//! Toolhelp 记录的父 pid 是**当时**的父进程号。父进程退出后，这个号码会被系统回收
//! 给新进程，于是"父 pid"会指向一个和原来毫无关系的进程。照着这个假关系去展开，
//! 就会把路由写到不相干的程序头上。
//!
//! 识破办法：比对创建时间。父进程一定**早于**子进程创建。若"父"的创建时间晚于子，
//! 那就是号码被复用了，这条边作废。
//!
//! 拿不到创建时间时（受保护进程拒绝 `OpenProcess`、进程已退出）**保留**这条边——
//! 宁可多认一个真父亲，也不要把正常的进程树剪断。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use super::ffi;

/// 一条进程快照记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// 只有文件名（如 `msedge.exe`），没有路径。
    pub exe: String,
}

/// 拍一张全进程快照。
///
/// 用 `CreateToolhelp32Snapshot` 而不是 WMI：它是同步的、不需要起 COM 服务、
/// 一次调用拿到全部进程，几百个进程也就一两毫秒。
pub fn snapshot() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    unsafe {
        let snap = ffi::CreateToolhelp32Snapshot(ffi::TH32CS_SNAPPROCESS, 0);
        if snap == ffi::INVALID_HANDLE_VALUE || snap == 0 {
            return out;
        }
        let mut pe: ffi::PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<ffi::PROCESSENTRY32W>() as u32;

        if ffi::Process32FirstW(snap, &mut pe) != 0 {
            loop {
                out.push(ProcEntry {
                    pid: pe.th32ProcessID,
                    ppid: pe.th32ParentProcessID,
                    exe: ffi::utf16_arr_to_string(&pe.szExeFile),
                });
                if ffi::Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        ffi::CloseHandle(snap);
    }
    out
}

/// 进程是否还存在（并且我们能查询它）。
pub fn exists(pid: u32) -> bool {
    unsafe {
        let h = ffi::OpenProcess(ffi::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return false;
        }
        ffi::CloseHandle(h);
        true
    }
}

/// 进程创建时间（`FILETIME` 原始值）。拿不到返回 `None`。
fn create_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = ffi::OpenProcess(ffi::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return None;
        }
        let mut created = ffi::FILETIME::default();
        let mut exited = ffi::FILETIME::default();
        let mut kern = ffi::FILETIME::default();
        let mut user = ffi::FILETIME::default();
        let ok = ffi::GetProcessTimes(h, &mut created, &mut exited, &mut kern, &mut user);
        ffi::CloseHandle(h);
        if ok == 0 {
            None
        } else {
            Some(created.as_u64())
        }
    }
}

/// 一次快照 + 父子索引。
///
/// 创建时间**惰性**读取并缓存：只在真正遍历到某条边时才去 `OpenProcess`。
/// 几百个进程里我们关心的往往只有十几个，没必要每个都问一遍。
#[derive(Debug, Default)]
pub struct ProcTree {
    entries: Vec<ProcEntry>,
    index: HashMap<u32, usize>,
    children: HashMap<u32, Vec<u32>>,
    /// 是否用创建时间去校验父子边。
    ///
    /// [`ProcTree::snapshot`] 造的是真树，校验开着；[`ProcTree::from_entries`] 是
    /// **离线**树（单元测试、以及任何拿不到真实进程信息的场合），校验关掉——
    /// 否则测试里编的假 pid 可能正好撞上本机真实进程号，去问到一对毫无关系的时间戳，
    /// 测试就变成了看运气。
    verify: bool,
    /// pid → 创建时间。`None` 表示问不到（不是"不存在"）。
    created: RefCell<HashMap<u32, Option<u64>>>,
}

impl Clone for ProcTree {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            index: self.index.clone(),
            children: self.children.clone(),
            verify: self.verify,
            created: RefCell::new(self.created.borrow().clone()),
        }
    }
}

impl ProcTree {
    /// 空树。拿不到快照时用（此时所有归属判定会退化成单进程行为）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 拍一张真实快照并构树，父子边用创建时间校验。
    pub fn snapshot() -> Self {
        Self::build(snapshot(), true)
    }

    /// 用给定的记录构一棵**离线**树，不做 PID 复用校验。
    ///
    /// 这是给测试和离线分析用的入口；生产路径请走 [`Self::snapshot`]。
    pub fn from_entries(entries: Vec<ProcEntry>) -> Self {
        Self::build(entries, false)
    }

    fn build(entries: Vec<ProcEntry>, verify: bool) -> Self {
        let index: HashMap<u32, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.pid, i))
            .collect();

        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in &entries {
            // ppid == 自身是坏数据（会造成自环）；ppid == 0 是"没有父"的哨兵值。
            if e.ppid != 0 && e.ppid != e.pid {
                children.entry(e.ppid).or_default().push(e.pid);
            }
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }

        Self {
            entries,
            index,
            children,
            verify,
            created: RefCell::new(HashMap::new()),
        }
    }

    /// 单元测试专用：预置创建时间，用来验证复用识破逻辑。
    #[cfg(test)]
    fn inject_times(&self, pairs: &[(u32, u64)]) {
        let mut m = self.created.borrow_mut();
        for &(pid, t) in pairs {
            m.insert(pid, Some(t));
        }
    }

    /// 单元测试专用：构一棵开着校验的离线树。
    #[cfg(test)]
    fn offline_verifying(entries: Vec<ProcEntry>) -> Self {
        Self::build(entries, true)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, pid: u32) -> Option<&ProcEntry> {
        self.index.get(&pid).map(|&i| &self.entries[i])
    }

    pub fn exe_of(&self, pid: u32) -> Option<&str> {
        self.get(pid).map(|e| e.exe.as_str())
    }

    pub fn ppid_of(&self, pid: u32) -> Option<u32> {
        self.get(pid).map(|e| e.ppid)
    }

    /// 直接子进程（**未经**复用校验）。展示用；展开路由请用 [`Self::descendants`]。
    pub fn children_of(&self, pid: u32) -> &[u32] {
        self.children.get(&pid).map(Vec::as_slice).unwrap_or(&[])
    }

    /// `pid` 的全部后代，不含自身。已剔除"父 pid 被复用"的伪边。
    ///
    /// 结果按 pid 升序，保证同样输入下顺序稳定。
    pub fn descendants(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut queue: VecDeque<u32> = VecDeque::new();

        seen.insert(pid);
        queue.push_back(pid);

        while let Some(p) = queue.pop_front() {
            for &c in self.children_of(p) {
                if !seen.insert(c) {
                    continue; // 防御性：坏数据造出的环
                }
                if !self.edge_ok(p, c) {
                    continue;
                }
                out.push(c);
                queue.push_back(c);
            }
        }
        out.sort_unstable();
        out
    }

    /// `ancestor` 是否是 `pid` 的祖先（严格上溯，不含自身）。
    pub fn is_ancestor(&self, ancestor: u32, pid: u32) -> bool {
        // 显式排除自身。不加这一条的话，坏数据造出的环（A 的父是 B、B 的父是 A）
        // 会让 A 变成自己的祖先。
        if ancestor == pid {
            return false;
        }
        let mut cur = pid;
        // 深度上限纯属防御：正常进程树不会有几百层，但坏数据可能造出长链。
        for _ in 0..256 {
            let Some(p) = self.ppid_of(cur) else { return false };
            if p == 0 || p == cur {
                return false;
            }
            if !self.edge_ok(p, cur) {
                return false;
            }
            if p == ancestor {
                return true;
            }
            cur = p;
        }
        false
    }

    /// 从 `pid` 一路向上，返回 `[父, 祖父, ...]`。
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = pid;
        for _ in 0..256 {
            let Some(p) = self.ppid_of(cur) else { break };
            if p == 0 || p == cur || !self.edge_ok(p, cur) {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    /// 这条父子边可信吗。
    ///
    /// 唯一会否决的情况：父的创建时间**晚于**子 —— 那说明 ppid 指的是另一个
    /// 借用了同一个号码的进程。问不到时间就放行（宁可多认一个真父亲，
    /// 也不要把正常的进程树剪断）。
    fn edge_ok(&self, parent: u32, child: u32) -> bool {
        if self.get(parent).is_none() {
            return false; // 父进程已经不在快照里
        }
        let (Some(pc), Some(cc)) = (self.created(parent), self.created(child)) else {
            return true;
        };
        pc <= cc
    }

    fn created(&self, pid: u32) -> Option<u64> {
        if let Some(&v) = self.created.borrow().get(&pid) {
            return v;
        }
        if !self.verify {
            return None; // 离线树：绝不碰真实系统
        }
        let v = create_time(pid);
        self.created.borrow_mut().insert(pid, v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(pid: u32, ppid: u32, exe: &str) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            exe: exe.to_string(),
        }
    }

    /// 一棵典型的多进程浏览器树。
    fn browser_tree() -> ProcTree {
        ProcTree::from_entries(vec![
            e(100, 4, "explorer.exe"),
            e(200, 100, "chrome.exe"),      // 主进程，窗口在这里
            e(201, 200, "chrome.exe"),      // renderer
            e(202, 200, "chrome.exe"),      // audio utility <- 真正出声的
            e(203, 200, "chrome.exe"),      // gpu
            e(204, 201, "chrome.exe"),      // renderer 的子进程（孙）
            e(300, 100, "notepad.exe"),
        ])
    }

    #[test]
    fn descendants_covers_the_whole_subtree() {
        let t = browser_tree();
        // 注意：没有创建时间可读（这些 pid 在本机不存在），所以边一律保留
        assert_eq!(t.descendants(200), vec![201, 202, 203, 204]);
        assert_eq!(t.descendants(201), vec![204]);
        assert!(t.descendants(300).is_empty());
    }

    #[test]
    fn is_ancestor_walks_up() {
        let t = browser_tree();
        assert!(t.is_ancestor(200, 202));
        assert!(t.is_ancestor(200, 204));
        assert!(t.is_ancestor(100, 300));
        assert!(!t.is_ancestor(202, 200)); // 反了
        assert!(!t.is_ancestor(200, 200)); // 不含自身
        assert!(!t.is_ancestor(300, 200)); // 不同分支
    }

    #[test]
    fn ancestors_lists_the_chain() {
        let t = browser_tree();
        assert_eq!(t.ancestors(204), vec![201, 200, 100]);
        assert_eq!(t.ancestors(200), vec![100]);
    }

    #[test]
    fn cycle_in_bad_data_does_not_hang() {
        // 坏数据：A 的父是 B，B 的父是 A
        let t = ProcTree::from_entries(vec![e(1, 2, "a.exe"), e(2, 1, "b.exe")]);
        let d = t.descendants(1);
        assert!(d.contains(&2));
        assert!(!t.is_ancestor(1, 1));
    }

    #[test]
    fn missing_parent_edge_is_dropped() {
        // 父进程不在快照里（早退出了）→ 这条边不可信
        let t = ProcTree::from_entries(vec![e(10, 999, "orphan.exe")]);
        assert!(t.descendants(999).is_empty());
        assert!(!t.is_ancestor(999, 10));
    }

    #[test]
    fn self_parent_is_ignored() {
        let t = ProcTree::from_entries(vec![e(5, 5, "weird.exe")]);
        assert!(t.descendants(5).is_empty());
    }

    #[test]
    fn reused_parent_pid_is_detected() {
        // 场景：child(50) 的父进程早就退了，号码 40 被一个**更晚**创建的进程占用。
        // 只看 ppid 会把它当成 40 的孩子；比对创建时间就能识破。
        let t = ProcTree::offline_verifying(vec![
            e(40, 1, "unrelated.exe"),
            e(50, 40, "child.exe"),
        ]);
        // 40 比 50 晚创建 → 不可能是父子
        t.inject_times(&[(40, 2_000), (50, 1_000)]);
        assert!(t.descendants(40).is_empty(), "伪边应被剔除");
        assert!(!t.is_ancestor(40, 50));
        assert!(t.ancestors(50).is_empty());
    }

    #[test]
    fn genuine_parent_edge_survives_verification() {
        let t = ProcTree::offline_verifying(vec![
            e(40, 1, "parent.exe"),
            e(50, 40, "child.exe"),
        ]);
        // 40 早于 50 → 真父子，保留
        t.inject_times(&[(40, 1_000), (50, 2_000)]);
        assert_eq!(t.descendants(40), vec![50]);
        assert!(t.is_ancestor(40, 50));
    }

    #[test]
    fn unknown_create_time_keeps_the_edge() {
        // 受保护进程问不到创建时间：宁可保留边，也不要剪断真实进程树
        let t = ProcTree::offline_verifying(vec![e(40, 1, "p.exe"), e(50, 40, "c.exe")]);
        t.inject_times(&[(40, 2_000)]); // 只注入了父，子问不到
        assert_eq!(t.descendants(40), vec![50]);
    }
}
