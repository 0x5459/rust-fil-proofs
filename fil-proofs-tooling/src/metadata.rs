use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// Captures metadata about the current setup.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Metadata<T> {
    git: GitMetadata,
    system: SystemMetadata,
    benchmarks: T,
}

impl<T> Metadata<T> {
    pub fn wrap(benchmarks: T) -> Result<Self> {
        Ok(Metadata {
            git: GitMetadata::new()?,
            system: SystemMetadata::new()?,
            benchmarks,
        })
    }
}

/// Captures git specific metadata about the current repo.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct GitMetadata {
    hash: String,
    date: DateTime<Utc>,
}

impl GitMetadata {
    pub fn new() -> Result<Self> {
        // Unwrap is OK as vergen returns a valid timestamp.
        let date = env!("VERGEN_GIT_COMMIT_TIMESTAMP")
            .parse::<DateTime<Utc>>()
            .expect("VERGEN_GIT_COMMIT_TIMESTAMP error");
        Ok(GitMetadata {
            hash: env!("VERGEN_GIT_SHA").to_string(),
            date,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SystemMetadata {
    system: String,
    release: String,
    version: String,
    architecture: String,
    processor: String,
    processor_base_frequency_hz: u16,
    processor_max_frequency_hz: u16,
    processor_features: String,
    processor_cores_logical: u64,
    processor_cores_physical: u64,
    memory_total_bytes: u64,
}

impl SystemMetadata {
    pub fn new() -> Result<Self> {
        use async_std::task::block_on;
        let host = block_on(async { heim::host::platform().await })
            .map_err(|_| anyhow!("Failed to retrieve host information"))?;
        let memory = block_on(async { heim::memory::memory().await })
            .map_err(|_| anyhow!("Failed to retrieve memory information"))?;
        let cpu_logical = block_on(async { heim::cpu::logical_count().await })
            .map_err(|_| anyhow!("Failed to retrieve cpu logical count information"))?;
        let cpu_physical = block_on(async { heim::cpu::physical_count().await })
            .map_err(|_| anyhow!("Failed to retrieve cpu physical count information"))?;

        let (processor, base, max, features) = {
            #[cfg(target_arch = "x86_64")]
            {
                let cpuid = raw_cpuid::CpuId::new();
                let processor = cpuid
                    .get_processor_brand_string()
                    .map(|s| s.as_str().to_owned())
                    .unwrap_or_default();
                let (base, max) = cpuid
                    .get_processor_frequency_info()
                    .map(|info| {
                        (
                            info.processor_base_frequency(),
                            info.processor_max_frequency(),
                        )
                    })
                    .unwrap_or_default();
                (
                    processor,
                    base,
                    max,
                    cpuid
                        .get_feature_info()
                        .map(|info| format!("{:?}", info))
                        .unwrap_or_default(),
                )
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                ("unknown".into(), 0, 0, "unknown".into())
            }
        };

        Ok(SystemMetadata {
            system: host.system().into(),
            release: host.release().into(),
            version: host.version().into(),
            architecture: host.architecture().as_str().into(),
            processor,
            processor_base_frequency_hz: base,
            processor_max_frequency_hz: max,
            processor_features: features,
            processor_cores_logical: cpu_logical,
            processor_cores_physical: cpu_physical.unwrap_or_default(),
            memory_total_bytes: memory.total().get::<heim::units::information::byte>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata() {
        let m = Metadata::wrap(()).expect("failed to create metadata");
        println!("{:#?}", m);

        assert!(m.system.memory_total_bytes > 0);
    }
}
