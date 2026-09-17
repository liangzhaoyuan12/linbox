//! inotify 限制调优的数据模型（纯数据，无 UI 依赖）。

/// 一次检测的完整结果。
#[derive(Clone, Debug)]
pub struct InotifyStatus {
    /// 当前生效的 `fs.inotify.max_user_watches`（来自 /proc，实时值）。
    pub watches: u64,
    /// 当前生效的 `fs.inotify.max_user_instances`（来自 /proc，实时值）。
    pub instances: u64,
    /// 持久化配置里的 watches 值（None = 没有持久化，重启会回落默认）。
    pub persisted_watches: Option<u64>,
    /// 持久化配置里的 instances 值。
    pub persisted_instances: Option<u64>,
    /// 持久化配置所在的文件（用于展示"生效来源"）。
    pub persisted_source: Option<String>,
}

impl InotifyStatus {
    /// watches 是否低于推荐档位（决定界面提示文案）。
    pub fn watches_low(&self) -> bool {
        self.watches < RECOMMENDED_WATCHES
    }

    /// 持久化的 watches 是否与当前生效值一致（不一致提示需写文件，否则重启回退）。
    pub fn watches_persisted_match(&self) -> bool {
        self.persisted_watches == Some(self.watches)
    }

    /// 持久化的 instances 是否与当前生效值一致。
    pub fn instances_persisted_match(&self) -> bool {
        self.persisted_instances == Some(self.instances)
    }
}

/// watches 推荐值（用户指定的目标档位；默认内核值 8192 远远不够 IDE 使用）。
pub const RECOMMENDED_WATCHES: u64 = 2_097_152;

/// instances 推荐值（默认 128 太小，每个监听进程吃掉一个实例）。
pub const RECOMMENDED_INSTANCES: u64 = 1024;
