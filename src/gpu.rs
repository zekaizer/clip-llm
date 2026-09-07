//! Which wgpu adapter the overlay renders with.
//!
//! eframe hands the adapter selector every adapter of every enabled backend,
//! so one GPU appears once per backend (Vulkan and DX12 on Windows). The
//! ranking is pure over [`AdapterInfo`] so it is testable without a GPU;
//! `main.rs` maps the live adapters through [`choose`].

use tracing::warn;
use wgpu::{AdapterInfo, DeviceType};

/// `[ui].gpu` — which adapter the overlay renders with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuPreference {
    /// Discrete GPU first, then integrated (default).
    HighPerformance,
    /// Integrated GPU first, then discrete.
    LowPower,
    /// Adapters whose name contains this text (case-insensitive) first.
    Name(String),
}

impl GpuPreference {
    /// Parse the config value: `"high-performance"` | `"low-power"`
    /// (case-insensitive, trimmed) or any other non-empty text as a name
    /// filter. Absent or blank = [`GpuPreference::HighPerformance`].
    pub fn parse(value: Option<&str>) -> Self {
        let text = value.map(str::trim).unwrap_or("");
        if text.is_empty() || text.eq_ignore_ascii_case("high-performance") {
            GpuPreference::HighPerformance
        } else if text.eq_ignore_ascii_case("low-power") {
            GpuPreference::LowPower
        } else {
            GpuPreference::Name(text.to_owned())
        }
    }
}

/// Index into `infos` of the adapter to use; `None` when `infos` is empty.
///
/// `composes_alpha[i]` says whether adapter `i`'s surface offers a
/// transparent composite-alpha mode (missing entries count as capable).
/// Hardware GPUs rank above `Other`/virtual adapters, and software (`Cpu`)
/// adapters come last so they are only ever a fallback; enumeration order
/// breaks ties. A name filter with no match falls back to the
/// high-performance order with a warning.
pub fn choose(
    infos: &[AdapterInfo],
    composes_alpha: &[bool],
    pref: &GpuPreference,
) -> Option<usize> {
    if infos.is_empty() {
        return None;
    }
    let alpha = |i: usize| composes_alpha.get(i).copied().unwrap_or(true);
    let all = || (0..infos.len()).collect::<Vec<usize>>();
    let (rank, mut candidates): (fn(DeviceType) -> u8, Vec<usize>) = match pref {
        GpuPreference::HighPerformance => (discrete_first, all()),
        GpuPreference::LowPower => (integrated_first, all()),
        GpuPreference::Name(text) => {
            let needle = text.to_lowercase();
            let matched: Vec<usize> = (0..infos.len())
                .filter(|&i| infos[i].name.to_lowercase().contains(&needle))
                .collect();
            if matched.is_empty() {
                warn!("[ui].gpu = {text:?} matches no adapter; using the high-performance order");
                (discrete_first, all())
            } else {
                (discrete_first, matched)
            }
        }
    };
    // The overlay is transparent: an adapter whose surface cannot compose
    // alpha renders it on a black panel, so such adapters are skipped while
    // any candidate can.
    if candidates.iter().any(|&i| alpha(i)) {
        candidates.retain(|&i| alpha(i));
    } else {
        warn!("no candidate wgpu adapter composes alpha; the overlay background may render opaque");
    }
    // `min_by_key` keeps the first of equal keys - enumeration order breaks ties.
    candidates
        .into_iter()
        .min_by_key(|&i| rank(infos[i].device_type))
}

/// High-performance order; `Other` may be a real GPU behind an API without
/// device types (GL), so it stays ahead of virtual and software adapters.
fn discrete_first(device_type: DeviceType) -> u8 {
    match device_type {
        DeviceType::DiscreteGpu => 0,
        DeviceType::IntegratedGpu => 1,
        DeviceType::Other => 2,
        DeviceType::VirtualGpu => 3,
        DeviceType::Cpu => 4,
    }
}

fn integrated_first(device_type: DeviceType) -> u8 {
    match device_type {
        DeviceType::IntegratedGpu => 0,
        DeviceType::DiscreteGpu => 1,
        other => discrete_first(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::Backend;

    /// Every adapter composes alpha.
    fn all(n: usize) -> Vec<bool> {
        vec![true; n]
    }

    fn info(name: &str, device_type: DeviceType, backend: Backend) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            vendor: 0,
            device: 0,
            device_type,
            driver: String::new(),
            driver_info: String::new(),
            backend,
        }
    }

    #[test]
    fn parse_keywords_case_insensitively_and_trimmed() {
        assert_eq!(GpuPreference::parse(None), GpuPreference::HighPerformance);
        assert_eq!(
            GpuPreference::parse(Some("")),
            GpuPreference::HighPerformance
        );
        assert_eq!(
            GpuPreference::parse(Some("  ")),
            GpuPreference::HighPerformance
        );
        assert_eq!(
            GpuPreference::parse(Some(" High-Performance ")),
            GpuPreference::HighPerformance
        );
        assert_eq!(
            GpuPreference::parse(Some("LOW-POWER")),
            GpuPreference::LowPower
        );
        assert_eq!(
            GpuPreference::parse(Some(" Intel Arc ")),
            GpuPreference::Name("Intel Arc".into())
        );
    }

    #[test]
    fn high_performance_prefers_discrete_then_integrated_then_other() {
        let infos = [
            info("igpu", DeviceType::IntegratedGpu, Backend::Vulkan),
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
            info("warp", DeviceType::Cpu, Backend::Dx12),
        ];
        assert_eq!(
            choose(&infos, &all(infos.len()), &GpuPreference::HighPerformance),
            Some(1)
        );
        assert_eq!(
            choose(&infos[..1], &all(1), &GpuPreference::HighPerformance),
            Some(0)
        );
        let no_gpu = [
            info("warp", DeviceType::Cpu, Backend::Dx12),
            info("gl", DeviceType::Other, Backend::Gl),
        ];
        assert_eq!(
            choose(&no_gpu, &all(2), &GpuPreference::HighPerformance),
            Some(1)
        );
    }

    #[test]
    fn low_power_prefers_integrated_over_discrete() {
        let infos = [
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
            info("igpu", DeviceType::IntegratedGpu, Backend::Vulkan),
        ];
        assert_eq!(
            choose(&infos, &all(infos.len()), &GpuPreference::LowPower),
            Some(1)
        );
        let no_igpu = [
            info("warp", DeviceType::Cpu, Backend::Dx12),
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
        ];
        assert_eq!(choose(&no_igpu, &all(2), &GpuPreference::LowPower), Some(1));
    }

    #[test]
    fn name_filter_matches_case_insensitively() {
        let infos = [
            info(
                "NVIDIA GeForce RTX 4070",
                DeviceType::DiscreteGpu,
                Backend::Vulkan,
            ),
            info(
                "Intel(R) UHD Graphics",
                DeviceType::IntegratedGpu,
                Backend::Vulkan,
            ),
        ];
        assert_eq!(
            choose(
                &infos,
                &all(infos.len()),
                &GpuPreference::Name("intel".into())
            ),
            Some(1)
        );
        assert_eq!(
            choose(
                &infos,
                &all(infos.len()),
                &GpuPreference::Name("UHD".into())
            ),
            Some(1)
        );
        assert_eq!(
            choose(
                &infos,
                &all(infos.len()),
                &GpuPreference::Name("geforce".into())
            ),
            Some(0)
        );
    }

    #[test]
    fn name_filter_without_match_falls_back_to_high_performance() {
        let infos = [
            info("igpu", DeviceType::IntegratedGpu, Backend::Vulkan),
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
        ];
        assert_eq!(
            choose(
                &infos,
                &all(infos.len()),
                &GpuPreference::Name("radeon".into())
            ),
            Some(1)
        );
    }

    #[test]
    fn opaque_only_adapters_are_skipped_when_another_composes_alpha() {
        // Windows lists a GPU once per backend; NVIDIA's Vulkan surface is
        // Opaque only while the DX12 DirectComposition surface composes alpha.
        let infos = [
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
            info("igpu", DeviceType::IntegratedGpu, Backend::Vulkan),
            info("dgpu", DeviceType::DiscreteGpu, Backend::Dx12),
            info("igpu", DeviceType::IntegratedGpu, Backend::Dx12),
        ];
        let alpha = [false, true, true, true];
        assert_eq!(
            choose(&infos, &alpha, &GpuPreference::HighPerformance),
            Some(2)
        );
        assert_eq!(choose(&infos, &alpha, &GpuPreference::LowPower), Some(1));
        assert_eq!(
            choose(&infos, &alpha, &GpuPreference::Name("dgpu".into())),
            Some(2)
        );
        // A black panel beats no panel only when nothing composes alpha:
        // then enumeration order stands.
        let none = [false, false, false, false];
        assert_eq!(
            choose(&infos, &none, &GpuPreference::HighPerformance),
            Some(0)
        );
        // An explicit name is honored even when only its opaque adapter matches.
        let only_dgpu_vulkan = [
            info("dgpu", DeviceType::DiscreteGpu, Backend::Vulkan),
            info("igpu", DeviceType::IntegratedGpu, Backend::Vulkan),
        ];
        assert_eq!(
            choose(
                &only_dgpu_vulkan,
                &[false, true],
                &GpuPreference::Name("dgpu".into())
            ),
            Some(0)
        );
    }

    #[test]
    fn software_adapter_is_the_last_resort_only() {
        let only_cpu = [info("warp", DeviceType::Cpu, Backend::Dx12)];
        assert_eq!(
            choose(&only_cpu, &all(1), &GpuPreference::LowPower),
            Some(0)
        );
        assert_eq!(choose(&[], &[], &GpuPreference::HighPerformance), None);
    }
}
