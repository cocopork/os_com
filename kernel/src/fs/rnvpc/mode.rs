// SPDX-License-Identifier: MPL-2.0

//! P0 开关：`rnvpc` / `rnvpc_extend` / `rtio`（默认全关，不改变任何既有路径）。
//!
//! 两个入口都归到 [`RnvpcModeConfig::resolve`]：
//!
//! - 内核 cmdline：`rnvpc=`、`rnvpc_extend=`、`rtio=`；
//! - ext4 挂载选项：同名 token（缺省时回落 cmdline 的取值）。
//!
//! `rnvpc` 与 `rnvpc_extend` 描述的是**互斥**的两条路线（porting.md §1：rNVPC =
//! WAL-protected PM PageCache；rNVPC-extend = 持久数据面 + DRAM 控制面），同时开
//! 返回 `EINVAL`，绝不静默取其一。`rtio` 与两者正交（是写回调度开关，两种路线都要
//! 用它），因此可与任一模式组合，单独开也合法。

use core::str::FromStr;

use spin::Once;

use crate::prelude::*;

/// 实验路线选择。默认 [`RnvpcMode::Native`]。
///
/// 命名对照：实现计划里记作 `RnvpcMode{DramNg, Rnvpc, RnvpcExtend}`，其中
/// `DramNg` 即这里的 `Native`（原生 DRAM PageCache 路径）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RnvpcMode {
    /// 全关：原生 DRAM PageCache 路径（**默认**；行为与既有 ext4 完全一致）。
    Native,
    /// `rnvpc=`：rNVPC —— per-inode WAL / CoW / 容量回压的 PageCache。
    Rnvpc,
    /// `rnvpc_extend=`：rNVPC-extend —— NVPage/NVBH 持久数据面 + DRAM 控制面。
    RnvpcExtend,
}

/// 解析后的开关组合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RnvpcModeConfig {
    mode: RnvpcMode,
    rtio: bool,
}

impl Default for RnvpcModeConfig {
    fn default() -> Self {
        Self {
            mode: RnvpcMode::Native,
            rtio: false,
        }
    }
}

impl RnvpcModeConfig {
    /// 三个开关 → 配置。
    ///
    /// `rnvpc` 与 `rnvpc_extend` 同时开 = `EINVAL`（互斥）；`rtio` 正交。
    pub(crate) fn resolve(rnvpc: bool, rnvpc_extend: bool, rtio: bool) -> Result<Self> {
        if rnvpc && rnvpc_extend {
            return_errno_with_message!(
                Errno::EINVAL,
                "rnvpc and rnvpc_extend are mutually exclusive"
            );
        }
        let mode = if rnvpc {
            RnvpcMode::Rnvpc
        } else if rnvpc_extend {
            RnvpcMode::RnvpcExtend
        } else {
            RnvpcMode::Native
        };
        Ok(Self { mode, rtio })
    }

    pub(crate) fn mode(self) -> RnvpcMode {
        self.mode
    }

    pub(crate) fn rtio(self) -> bool {
        self.rtio
    }

    /// 内核 cmdline 入口：三个开关缺省即关。
    pub(crate) fn from_command_line() -> Result<Self> {
        Self::resolve(
            CMD_RNVPC.get().is_some_and(|param| param.0),
            CMD_RNVPC_EXTEND.get().is_some_and(|param| param.0),
            CMD_RTIO.get().is_some_and(|param| param.0),
        )
    }
}

/// `on/off` 取值解析（`1`/`true`/`on`/`yes`/`enabled` 为开）。
///
/// cmdline 参数与 ext4 挂载选项共用一份判定，避免两处写法漂移。
pub(crate) fn parse_onoff(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "on" | "yes" | "enabled")
}

/// `on/off` 型 cmdline 参数：`1`/`true`/`on`/`yes` 为开，其余为关（宽松解析，与
/// ext4 挂载选项既有的"未知取值保持默认"策略保持一致）。
#[derive(Debug)]
struct OnOffParam(bool);

impl FromStr for OnOffParam {
    type Err = core::convert::Infallible;

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        Ok(Self(parse_onoff(value)))
    }
}

static CMD_RNVPC: Once<OnOffParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc", CMD_RNVPC);

static CMD_RNVPC_EXTEND: Once<OnOffParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_extend", CMD_RNVPC_EXTEND);

static CMD_RTIO: Once<OnOffParam> = Once::new();
aster_cmdline::define_kv_param!("rtio", CMD_RTIO);
