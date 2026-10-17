// SPDX-License-Identifier: MPL-2.0

//! Configuration for simulated RNVPC memory.

use core::{
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
};

use aster_cmdline::{parse::ParseParamValue, types::MetricU64};
use spin::Once;

use crate::prelude::*;

#[derive(Debug)]
struct RawParam(String);

impl FromStr for RawParam {
    type Err = core::convert::Infallible;

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

// ── 规范名（T0.1b 起）：`rnvpc_*` ──
static RNVPC_SIZE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_size", RNVPC_SIZE);

static RNVPC_META_RESERVE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_meta_reserve", RNVPC_META_RESERVE);

static RNVPC_DATA_LIMIT: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_data_limit", RNVPC_DATA_LIMIT);

static RNVPC_SIM_MODE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_sim_mode", RNVPC_SIM_MODE);

/// Recovery-scaling experiment only: keep committed pages resident instead of
/// starting rTIO at the normal high watermark. The hard pool/data limits still
/// apply, so the workload stops with ENOSPC rather than silently exceeding the
/// configured 16-GiB image.
static RNVPC_RECOVERY_BENCH: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("rnvpc_recovery_bench", RNVPC_RECOVERY_BENCH);

// ── 旧名（改名前的 `nv_*`）：仍可读，读到即告警一次，便于已有脚本/实验目录平滑
// 迁移；规范名一旦出现，旧名被忽略（不冲突、不合并）──
static NV_POOL_SIZE_LEGACY: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_pool_size", NV_POOL_SIZE_LEGACY);

static NV_META_RESERVE_LEGACY: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_meta_reserve", NV_META_RESERVE_LEGACY);

static NV_DATA_LIMIT_LEGACY: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_data_limit", NV_DATA_LIMIT_LEGACY);

static NV_SIM_MODE_LEGACY: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_sim_mode", NV_SIM_MODE_LEGACY);

/// 旧名告警只打一次（每次启动），避免逐个参数刷屏。
static LEGACY_CMDNAME_WARNED: AtomicBool = AtomicBool::new(false);

/// 参数取值：规范名优先；只有旧名时沿用旧值并告警一次。
///
/// 抽成纯函数以便单测（不触全局 `Once`/cmdline）。
pub(super) fn select_param<'a>(
    canonical: Option<&'a str>,
    legacy: Option<&'a str>,
    canonical_name: &'static str,
) -> Option<&'a str> {
    match (canonical, legacy) {
        (Some(value), _) => Some(value),
        (None, Some(value)) => {
            if !LEGACY_CMDNAME_WARNED.swap(true, Ordering::AcqRel) {
                warn!(
                    "rNVPC: 内核 cmdline 参数已改名为 `{}=`；旧名仍可用但后续会移除，\
                     本次沿用旧值",
                    canonical_name
                );
            }
            Some(value)
        }
        (None, None) => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NvSimMode {
    Fast,
    /// Performance ablation only; skips simulated persistence operations.
    FastNoPersist,
    /// Reserved for explicit ktest fixtures; normal backend allocation rejects it.
    Model,
}

pub(crate) fn recovery_benchmark_enabled() -> bool {
    RNVPC_RECOVERY_BENCH
        .get()
        .is_some_and(|value| super::mode::parse_onoff(&value.0))
}

impl FromStr for NvSimMode {
    type Err = ();

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        match value {
            "fast" => Ok(Self::Fast),
            "fast-no-persist" => Ok(Self::FastNoPersist),
            "model" => Ok(Self::Model),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RnvpcConfig {
    total_pages: usize,
    meta_reserve_pages: usize,
    data_limit_pages: usize,
    mode: NvSimMode,
}

impl RnvpcConfig {
    pub(super) fn from_command_line() -> Result<Option<Self>> {
        // 规范名 `rnvpc_*` 优先；缺失时回落到旧名 `nv_*`（告警一次）。
        Self::from_params(
            select_param(
                RNVPC_SIZE.get().map(|value| value.0.as_str()),
                NV_POOL_SIZE_LEGACY.get().map(|value| value.0.as_str()),
                "rnvpc_size",
            ),
            select_param(
                RNVPC_META_RESERVE.get().map(|value| value.0.as_str()),
                NV_META_RESERVE_LEGACY.get().map(|value| value.0.as_str()),
                "rnvpc_meta_reserve",
            ),
            select_param(
                RNVPC_DATA_LIMIT.get().map(|value| value.0.as_str()),
                NV_DATA_LIMIT_LEGACY.get().map(|value| value.0.as_str()),
                "rnvpc_data_limit",
            ),
            select_param(
                RNVPC_SIM_MODE.get().map(|value| value.0.as_str()),
                NV_SIM_MODE_LEGACY.get().map(|value| value.0.as_str()),
                "rnvpc_sim_mode",
            ),
        )
    }

    pub(crate) fn from_params(
        total: Option<&str>,
        meta_reserve: Option<&str>,
        data_limit: Option<&str>,
        mode: Option<&str>,
    ) -> Result<Option<Self>> {
        fn parse_bytes(value: Option<&str>, name: &'static str) -> Result<Option<usize>> {
            value
                .map(|raw| {
                    let bytes = MetricU64::parse_param(raw)
                        .map_err(|_| Error::with_message(Errno::EINVAL, name))?
                        .0;
                    usize::try_from(bytes).map_err(|_| Error::with_message(Errno::EOVERFLOW, name))
                })
                .transpose()
        }

        let mode = mode
            .map(|value| {
                value
                    .parse::<NvSimMode>()
                    .map_err(|_| Error::with_message(Errno::EINVAL, "invalid rnvpc_sim_mode value"))
            })
            .transpose()?;
        Self::from_bytes(
            parse_bytes(total, "invalid rnvpc_size value")?,
            parse_bytes(meta_reserve, "invalid rnvpc_meta_reserve value")?,
            parse_bytes(data_limit, "invalid rnvpc_data_limit value")?,
            mode,
        )
    }

    pub(crate) fn from_bytes(
        total_bytes: Option<usize>,
        meta_reserve_bytes: Option<usize>,
        data_limit_bytes: Option<usize>,
        mode: Option<NvSimMode>,
    ) -> Result<Option<Self>> {
        let Some(total_bytes) = total_bytes else {
            if meta_reserve_bytes.is_some() || data_limit_bytes.is_some() || mode.is_some() {
                return_errno_with_message!(Errno::EINVAL, "RNVPC options require rnvpc_size");
            }
            return Ok(None);
        };
        let Some(meta_reserve_bytes) = meta_reserve_bytes else {
            return_errno_with_message!(Errno::EINVAL, "rnvpc_meta_reserve is required");
        };
        let Some(data_limit_bytes) = data_limit_bytes else {
            return_errno_with_message!(Errno::EINVAL, "rnvpc_data_limit is required");
        };

        if total_bytes == 0
            || meta_reserve_bytes == 0
            || !total_bytes.is_multiple_of(PAGE_SIZE)
            || !meta_reserve_bytes.is_multiple_of(PAGE_SIZE)
            || !data_limit_bytes.is_multiple_of(PAGE_SIZE)
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC total and metadata reserve must be nonzero; all capacities must be page aligned"
            );
        }

        let total_pages = total_bytes / PAGE_SIZE;
        let meta_reserve_pages = meta_reserve_bytes / PAGE_SIZE;
        let data_limit_pages = data_limit_bytes / PAGE_SIZE;
        if total_pages > u32::MAX as usize + 1 {
            return_errno_with_message!(
                Errno::EOVERFLOW,
                "RNVPC capacity exceeds the pool-local ID space"
            );
        }
        if meta_reserve_pages > total_pages || data_limit_pages > total_pages - meta_reserve_pages {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC metadata reserve and data limit exceed total capacity"
            );
        }

        Ok(Some(Self {
            total_pages,
            meta_reserve_pages,
            data_limit_pages,
            mode: mode.unwrap_or(NvSimMode::Fast),
        }))
    }

    pub(crate) fn total_pages(self) -> usize {
        self.total_pages
    }

    pub(crate) fn meta_reserve_pages(self) -> usize {
        self.meta_reserve_pages
    }

    pub(crate) fn data_limit_pages(self) -> usize {
        self.data_limit_pages
    }

    pub(crate) fn mode(self) -> NvSimMode {
        self.mode
    }
}

#[cfg(ktest)]
mod recovery_tests {
    use super::*;

    #[ostd::prelude::ktest]
    fn recovered_config_mode_and_canonical_precedence() {
        assert_eq!(
            "fast-no-persist".parse::<NvSimMode>(),
            Ok(NvSimMode::FastNoPersist)
        );
        assert_eq!(
            select_param(Some("8M"), Some("4M"), "rnvpc_size"),
            Some("8M")
        );
        assert_eq!(select_param(None, Some("4M"), "rnvpc_size"), Some("4M"));
        assert!(RnvpcConfig::from_params(None, None, None, Some("fast-no-persist")).is_err());
        let config =
            RnvpcConfig::from_params(Some("8M"), Some("1M"), Some("7M"), Some("fast-no-persist"))
                .unwrap()
                .unwrap();
        assert_eq!(config.mode(), NvSimMode::FastNoPersist);
        assert_eq!(config.total_pages(), 8 * 1024 * 1024 / PAGE_SIZE);
    }
}
