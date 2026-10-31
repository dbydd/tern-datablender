use std::sync::Arc;
use std::path::Path;
use anyhow::Result;

use crate::model::{DatasetInfo, Grid, Slice};
use crate::netcdf_io;
use crate::hdf5_io;
use crate::mat_io;

#[derive(Debug)]
pub enum Dataset {
    NetCdf(netcdf_io::Dataset),
    Hdf5(hdf5_io::Hdf5Dataset),
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
        } else if ext == "h5" || ext == "hdf5" {
            let h5 = hdf5_io::Hdf5Dataset::open(path)?;
            Ok(Self::Hdf5(h5))
        } else {
            // Default to NetCDF, fallback to HDF5 or MAT if NetCDF fails
            match netcdf_io::Dataset::open(path) {
                Ok(nc) => Ok(Self::NetCdf(nc)),
                Err(err) => {
                    if let Ok(h5) = hdf5_io::Hdf5Dataset::open(path) {
                        Ok(Self::Hdf5(h5))
                    } else if let Ok(mat) = mat_io::MatDataset::open(path) {
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
            Self::Hdf5(h5) => h5.info(),
            Self::Mat(mat) => mat.info(),
        }
    }

    pub fn default_slice(&self, variable: &str) -> Result<Slice> {
        match self {
            Self::NetCdf(nc) => nc.default_slice(variable),
            Self::Hdf5(h5) => h5.default_slice(variable),
            Self::Mat(mat) => mat.default_slice(variable),
        }
    }

    pub fn read(&self, variable: &str, slice: &Slice) -> Result<Arc<Grid>> {
        match self {
            Self::NetCdf(nc) => nc.read(variable, slice),
            Self::Hdf5(h5) => h5.read(variable, slice),
            Self::Mat(mat) => mat.read(variable, slice),
        }
    }

    pub fn read_time_shift(&self, variable: &str, slice: &Slice, steps: i64) -> Result<Arc<Grid>> {
        match self {
            Self::NetCdf(nc) => nc.read_time_shift(variable, slice, steps),
            Self::Hdf5(h5) => h5.read_time_shift(variable, slice, steps),
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
            Self::Hdf5(h5) => h5.series(variable, slice, dimension, x, y),
            Self::Mat(mat) => mat.series(variable, slice, dimension, x, y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_glorys_h5() {
        let path = "data/glorys1997_ocean_128.h5";
        if !Path::new(path).exists() {
            return;
        }
        let ds = Dataset::open(path).expect("open glorys h5");
        let info = ds.info();
        assert!(info.variables.iter().any(|v| v.name == "uo_data"));
        assert!(info.variables.iter().any(|v| v.name == "vo_data"));
        assert!(info.variables.iter().any(|v| v.name == "uv_data"));

        let slice = ds.default_slice("uo_data").expect("default slice");
        assert_eq!(slice.x, "lon");
        assert_eq!(slice.y, "lat");

        let grid = ds.read("uo_data", &slice).expect("read grid");
        assert_eq!(grid.width, 128);
        assert_eq!(grid.height, 128);
        assert_eq!(grid.values.len(), 128 * 128);
    }
}
