use std::sync::Arc;
use std::path::Path;
use anyhow::Result;

use crate::model::{DatasetInfo, Grid, Slice};
use crate::netcdf_io;
use crate::mat_io;

#[derive(Debug)]
pub enum Dataset {
    NetCdf(netcdf_io::Dataset),
    Mat(mat_io::MatDataset),
}

impl Dataset {
    pub fn open(path: &str) -> Result<Self> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if ext == "mat" {
            let mat = mat_io::MatDataset::open(path)?;
            Ok(Self::Mat(mat))
        } else {
            // Default to NetCDF, fallback to MAT if NetCDF fails on valid MAT
            match netcdf_io::Dataset::open(path) {
                Ok(nc) => Ok(Self::NetCdf(nc)),
                Err(err) => {
                    if let Ok(mat) = mat_io::MatDataset::open(path) {
                        Ok(Self::Mat(mat))
                    } else {
                        Err(err)
                    }
                }
            }
        }
    }

    pub fn info(&self) -> &DatasetInfo {
        match self {
            Self::NetCdf(nc) => nc.info(),
            Self::Mat(mat) => mat.info(),
        }
    }

    pub fn default_slice(&self, variable: &str) -> Result<Slice> {
        match self {
            Self::NetCdf(nc) => nc.default_slice(variable),
            Self::Mat(mat) => mat.default_slice(variable),
        }
    }

    pub fn read(&self, variable: &str, slice: &Slice) -> Result<Arc<Grid>> {
        match self {
            Self::NetCdf(nc) => nc.read(variable, slice),
            Self::Mat(mat) => mat.read(variable, slice),
        }
    }

    pub fn read_time_shift(&self, variable: &str, slice: &Slice, steps: i64) -> Result<Arc<Grid>> {
        match self {
            Self::NetCdf(nc) => nc.read_time_shift(variable, slice, steps),
            Self::Mat(mat) => mat.read_time_shift(variable, slice, steps),
        }
    }

    pub fn series(
        &self,
        variable: &str,
        slice: &Slice,
        dimension: &str,
        x: usize,
        y: usize,
    ) -> Result<Vec<Option<f64>>> {
        match self {
            Self::NetCdf(nc) => nc.series(variable, slice, dimension, x, y),
            Self::Mat(mat) => mat.series(variable, slice, dimension, x, y),
        }
    }
}
