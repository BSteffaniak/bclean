//! History sources consumed by `TIME_TO_FULL` and `GROWTH` triggers.
//!
//! The index crate implements these against its SQLite rollups; tests use
//! [`MemoryHistory`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One volume usage sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSample {
    /// Sample time (Unix seconds).
    pub at: i64,
    /// Used bytes.
    pub used: u64,
}

/// One directory size sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeSample {
    /// Sample time (Unix seconds).
    pub at: i64,
    /// Directory size in bytes.
    pub bytes: u64,
}

/// Recent volume usage history.
pub trait VolumeHistory {
    /// Samples for the volume mounted at `mount_point` taken at or after `since`, in any order.
    fn volume_samples(&self, mount_point: &str, since: i64) -> Vec<VolumeSample>;
}

/// Directory size history.
pub trait GrowthHistory {
    /// Size samples for `path` taken at or after `since`, in any order.
    fn size_samples(&self, path: &Path, since: i64) -> Vec<SizeSample>;
}

/// History source with no data.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHistory;

impl VolumeHistory for NoHistory {
    fn volume_samples(&self, _mount_point: &str, _since: i64) -> Vec<VolumeSample> {
        Vec::new()
    }
}

impl GrowthHistory for NoHistory {
    fn size_samples(&self, _path: &Path, _since: i64) -> Vec<SizeSample> {
        Vec::new()
    }
}

/// In-memory history, useful for tests and for daemons without an index.
#[derive(Debug, Clone, Default)]
pub struct MemoryHistory {
    /// Volume samples by mount point.
    pub volumes: BTreeMap<String, Vec<VolumeSample>>,
    /// Directory samples by path.
    pub dirs: BTreeMap<PathBuf, Vec<SizeSample>>,
}

impl MemoryHistory {
    /// Records a volume sample.
    pub fn record_volume(&mut self, mount_point: &str, at: i64, used: u64) {
        self.volumes
            .entry(mount_point.to_owned())
            .or_default()
            .push(VolumeSample { at, used });
    }

    /// Records a directory size sample.
    pub fn record_dir(&mut self, path: impl Into<PathBuf>, at: i64, bytes: u64) {
        self.dirs
            .entry(path.into())
            .or_default()
            .push(SizeSample { at, bytes });
    }
}

impl VolumeHistory for MemoryHistory {
    fn volume_samples(&self, mount_point: &str, since: i64) -> Vec<VolumeSample> {
        self.volumes
            .get(mount_point)
            .map(|v| v.iter().copied().filter(|s| s.at >= since).collect())
            .unwrap_or_default()
    }
}

impl GrowthHistory for MemoryHistory {
    fn size_samples(&self, path: &Path, since: i64) -> Vec<SizeSample> {
        self.dirs
            .get(path)
            .map(|v| v.iter().copied().filter(|s| s.at >= since).collect())
            .unwrap_or_default()
    }
}

/// Least-squares linear fit `y = intercept + slope * x` over `(x, y)` points.
///
/// Returns `None` with fewer than two points or when all `x` are equal. `x` values are
/// centred before fitting for numerical stability.
#[must_use]
#[allow(clippy::cast_precision_loss)] // timestamps and byte counts fit comfortably in f64
pub fn linear_fit(points: &[(i64, u64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| *x as f64).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| *y as f64).sum::<f64>() / n;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (x, y) in points {
        let dx = *x as f64 - mean_x;
        sxx = dx.mul_add(dx, sxx);
        sxy = dx.mul_add(*y as f64 - mean_y, sxy);
    }
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    Some((mean_x.mul_add(-slope, mean_y), slope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_a_line() {
        let (intercept, slope) = linear_fit(&[(0, 10), (10, 30), (20, 50)]).unwrap();
        assert!((slope - 2.0).abs() < 1e-9);
        assert!((intercept - 10.0).abs() < 1e-9);
        assert!(linear_fit(&[(5, 1), (5, 2)]).is_none());
        assert!(linear_fit(&[(5, 1)]).is_none());
    }
}
