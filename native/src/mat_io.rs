use std::{
    collections::{BTreeMap, HashMap},
    fs::File as StdFile,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};
use anyhow::{ensure, Context, Result};
use matfile::{MatFile, NumericData};
use hdf5_metno::types::TypeDescriptor;
use hdf5_metno::{File as H5File, Hyperslab, Selection, SliceOrIndex};

use crate::model::{Axis, DatasetInfo, Dimension, Grid, Slice, Variable};

#[derive(Debug)]
pub struct MatDataset {
    info: DatasetInfo,
    backend: MatBackend,
    coords: HashMap<String, Vec<f64>>,
}

#[derive(Debug)]
enum MatBackend {
    V5(HashMap<String, MatVarV5>),
    V73(String),
}

#[derive(Clone, Debug)]
struct MatVarV5 {
    shape: Vec<usize>,
    data: Vec<f64>,
}

impl MatDataset {
    pub fn open(path: &str) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "MAT path is empty");
        let mut file = StdFile::open(path).with_context(|| format!("cannot open MAT file {path}"))?;
        let mut header = [0u8; 128];
        let bytes_read = file.read(&mut header).unwrap_or(0);
        ensure!(bytes_read >= 8, "file {path} is too small to be a MAT file");

        // Check if v7.3 (HDF5 magic at 0 or 512)
        let is_v73 = if &header[0..8] == b"\x89HDF\r\n\x1a\n" {
            true
        } else if let Ok(mut f) = StdFile::open(path) {
            f.seek(SeekFrom::Start(512)).is_ok() && {
                let mut hdf5_magic = [0u8; 8];
                f.read_exact(&mut hdf5_magic).is_ok() && &hdf5_magic == b"\x89HDF\r\n\x1a\n"
            }
        } else {
            false
        };

        if is_v73 {
            Self::open_v73(path)
        } else {
            Self::open_v5(path)
        }
    }

    fn open_v5(path: &str) -> Result<Self> {
        let file = StdFile::open(path).with_context(|| format!("cannot open MAT v5 file {path}"))?;
        let mat = MatFile::parse(file).with_context(|| format!("cannot parse MAT v5 file {path}"))?;
        let mut coords = HashMap::new();
        for array in mat.arrays() {
            let size = array.size();
            if size.len() == 2 && (size[0] == 1 || size[1] == 1) {
                let len = size[0].max(size[1]);
                if len > 1 {
                    if let Some(data) = extract_numeric_data(array.data()) {
                        coords.insert(array.name().to_string(), data);
                    }
                }
            }
        }

        let mut vars = HashMap::new();
        let mut variables = Vec::new();

        for array in mat.arrays() {
            let name = array.name().to_string();
            let size = array.size();
            let raw_data = match extract_numeric_data(array.data()) {
                Some(data) if !data.is_empty() => data,
                _ => continue,
            };

            // Exclude arrays with fewer than 2 dimensions or total elements 0
            if size.len() < 2 || size.iter().any(|&d| d == 0) || (size.len() == 2 && (size[0] == 1 || size[1] == 1)) {
                continue;
            }

            // In MATLAB, 2D matrix [rows, cols] has size [rows, cols]
            let shape = size.to_vec();
            let mut dimensions = Vec::with_capacity(shape.len());
            for (i, &len) in shape.iter().enumerate() {
                let dim_name = guess_dim_name_with_coords(i, len, shape.len(), &coords);
                dimensions.push(Dimension {
                    name: dim_name,
                    len,
                    units: String::new(),
                });
            }

            variables.push(Variable {
                name: name.clone(),
                dimensions,
                units: String::new(),
                long_name: name.clone(),
            });

            vars.insert(name.clone(), MatVarV5 {
                shape,
                data: raw_data,
            });
        }

        variables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            info: DatasetInfo {
                path: path.to_string(),
                variables,
            },
            backend: MatBackend::V5(vars),
            coords,
        })
    }

    fn open_v73(path: &str) -> Result<Self> {
        let h5 = H5File::open(path).with_context(|| format!("cannot open MAT v7.3 HDF5 file {path}"))?;
        let mut coords = HashMap::new();
        for name in h5.member_names()? {
            if name.starts_with('#') { continue; }
            let Ok(dset) = h5.dataset(&name) else { continue };
            let shape = dset.shape();
            let len = if shape.len() == 1 { shape[0] } else if shape.len() == 2 && (shape[0] == 1 || shape[1] == 1) { shape[0].max(shape[1]) } else { 0 };
            if len > 1 {
                if let Ok(arr) = dset.read_1d::<f64>() {
                    coords.insert(name.clone(), arr.to_vec());
                }
            }
        }

        let mut variables = Vec::new();

        for name in h5.member_names()? {
            if name.starts_with('#') {
                continue;
            }
            let Ok(dset) = h5.dataset(&name) else { continue };
            if dset.attr("MATLAB_empty").is_ok() {
                continue;
            }
            let hdf5_shape = dset.shape();
            if hdf5_shape.len() < 2 || hdf5_shape.iter().any(|&d| d == 0) || (hdf5_shape.len() == 2 && (hdf5_shape[0] == 1 || hdf5_shape[1] == 1)) {
                continue;
            }
            let Ok(dtype) = dset.dtype() else { continue };
            let Ok(desc) = dtype.to_descriptor() else { continue };
            if matches!(desc, TypeDescriptor::Compound(_)) {
                continue;
            }

            let mut matlab_shape = hdf5_shape;
            matlab_shape.reverse();

            let mut dimensions = Vec::with_capacity(matlab_shape.len());
            for (i, &len) in matlab_shape.iter().enumerate() {
                let dim_name = guess_dim_name_with_coords(i, len, matlab_shape.len(), &coords);
                dimensions.push(Dimension {
                    name: dim_name,
                    len,
                    units: String::new(),
                });
            }

            variables.push(Variable {
                name: name.clone(),
                dimensions,
                units: String::new(),
                long_name: name.clone(),
            });
        }

        variables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            info: DatasetInfo {
                path: path.to_string(),
                variables,
            },
            backend: MatBackend::V73(path.to_string()),
            coords,
        })
    }

    pub fn info(&self) -> &DatasetInfo {
        &self.info
    }

    pub fn default_slice(&self, variable: &str) -> Result<Slice> {
        let var = self
            .info
            .variables
            .iter()
            .find(|v| v.name == variable)
            .with_context(|| format!("variable {variable:?} not found"))?;
        let dims = &var.dimensions;
        ensure!(dims.len() >= 2, "variable {variable:?} has fewer than 2 dimensions");

        // Pick spatial axes: try to find X (lon) and Y (lat), otherwise last two dimensions
        let mut x = None;
        let mut y = None;
        for (i, d) in dims.iter().enumerate() {
            let lower = d.name.to_ascii_lowercase();
            if (lower.contains("lon") || lower == "x") && x.is_none() {
                x = Some(i);
            } else if (lower.contains("lat") || lower == "y") && y.is_none() {
                y = Some(i);
            }
        }
        let (px, py) = match (x, y) {
            (Some(px), Some(py)) if px != py => (px, py),
            _ => {
                let non_time: Vec<usize> = dims.iter().enumerate()
                    .filter(|(_, d)| !is_time_dim(&d.name))
                    .map(|(i, _)| i)
                    .collect();
                if non_time.len() >= 2 {
                    (non_time[non_time.len() - 1], non_time[non_time.len() - 2])
                } else {
                    (dims.len() - 1, dims.len() - 2)
                }
            }
        };

        let mut indices = BTreeMap::new();
        for (i, d) in dims.iter().enumerate() {
            if i != px && i != py {
                indices.insert(d.name.clone(), 0);
            }
        }
        Ok(Slice {
            x: dims[px].name.clone(),
            y: dims[py].name.clone(),
            indices,
        })
    }

    pub fn read(&self, variable: &str, slice: &Slice) -> Result<Arc<Grid>> {
        self.read_time_shift(variable, slice, 0)
    }

    pub fn read_time_shift(&self, variable: &str, slice: &Slice, steps: i64) -> Result<Arc<Grid>> {
        let var = self
            .info
            .variables
            .iter()
            .find(|v| v.name == variable)
            .with_context(|| format!("variable {variable:?} not found"))?;
        let dims = &var.dimensions;
        let px = dims.iter().position(|d| d.name == slice.x).context("slice.x not found")?;
        let py = dims.iter().position(|d| d.name == slice.y).context("slice.y not found")?;
        let width = dims[px].len;
        let height = dims[py].len;
        ensure!(width > 0 && height > 0, "plane has 0 size");

        // If time shifting is requested
        let mut indices = slice.indices.clone();
        if steps != 0 {
            let time_dim = dims.iter().find(|d| is_time_dim(&d.name))
                .with_context(|| format!("Time shift requires a time dimension in {variable:?}"))?;
            ensure!(time_dim.name != slice.x && time_dim.name != slice.y, "Time dimension cannot be a plotted axis");
            let at = indices.get(&time_dim.name).copied().unwrap_or(0);
            let shifted = i128::try_from(at)? + i128::from(steps);
            if shifted < 0 || shifted >= i128::try_from(time_dim.len)? {
                let x_axis = self.make_axis(&slice.x, width);
                let y_axis = self.make_axis(&slice.y, height);
                return Ok(Arc::new(Grid {
                    width,
                    height,
                    values: vec![f32::NAN; width * height],
                    x: x_axis,
                    y: y_axis,
                    units: String::new(),
                }));
            }
            indices.insert(time_dim.name.clone(), usize::try_from(shifted)?);
        }

        match &self.backend {
            MatBackend::V5(vars) => {
                let mat_var = vars.get(variable).context("variable not in v5 store")?;
                let shape = &mat_var.shape;
                let mut fixed_indices = Vec::with_capacity(shape.len());
                for (i, d) in dims.iter().enumerate() {
                    if i == px || i == py {
                        fixed_indices.push(None);
                    } else {
                        let idx = indices.get(&d.name).copied().unwrap_or(0).min(d.len - 1);
                        fixed_indices.push(Some(idx));
                    }
                }

                // In MATLAB (column-major / Fortran order):
                // Linear index = i0 + i1*S0 + i2*S0*S1 + ...
                let mut strides = vec![1usize; shape.len()];
                for i in 1..shape.len() {
                    strides[i] = strides[i - 1] * shape[i - 1];
                }

                let mut base_offset = 0;
                for (i, opt) in fixed_indices.iter().enumerate() {
                    if let Some(idx) = opt {
                        base_offset += idx * strides[i];
                    }
                }

                let x_stride = strides[px];
                let y_stride = strides[py];
                let mut values = vec![0.0f32; width * height];
                for y in 0..height {
                    for x in 0..width {
                        let offset = base_offset + x * x_stride + y * y_stride;
                        let val = mat_var.data.get(offset).copied().unwrap_or(f64::NAN);
                        values[y * width + x] = val as f32;
                    }
                }

                let x_axis = self.make_axis(&slice.x, width);
                let y_axis = self.make_axis(&slice.y, height);
                Ok(Arc::new(Grid { width, height, values, x: x_axis, y: y_axis, units: String::new() }))
            }
            MatBackend::V73(file_path) => {
                let h5 = H5File::open(file_path)?;
                let dset = h5.dataset(variable)?;
                let ndim = dset.ndim();
                let mut slices: Vec<SliceOrIndex> = Vec::with_capacity(ndim);

                // Map MATLAB dimension k to HDF5 axis (ndim - 1 - k)
                for hdf5_axis in 0..ndim {
                    let matlab_axis = ndim - 1 - hdf5_axis;
                    if matlab_axis == px || matlab_axis == py {
                        slices.push((..).into());
                    } else {
                        let dim_name = &dims[matlab_axis].name;
                        let at = indices.get(dim_name).copied().unwrap_or(0).min(dims[matlab_axis].len - 1);
                        slices.push(SliceOrIndex::Index(at));
                    }
                }

                let hyper: Hyperslab = slices.into();
                let selection: Selection = hyper.into();
                let raw_arr = dset.read_slice::<f64, _, ndarray::IxDyn>(selection)?;

                // raw_arr has shape of the sliced plane in HDF5 order
                // If px < py in MATLAB, then in HDF5 py_hdf5 < px_hdf5
                let raw_slice = raw_arr.as_slice().context("array is not contiguous")?;
                let mut values = vec![0.0f32; width * height];
                if px < py {
                    // HDF5 shape is [height, width]
                    for (i, &v) in raw_slice.iter().enumerate().take(width * height) {
                        values[i] = v as f32;
                    }
                } else {
                    // HDF5 shape is [width, height], transpose to [height, width]
                    for x in 0..width {
                        for y in 0..height {
                            let v = raw_slice[x * height + y];
                            values[y * width + x] = v as f32;
                        }
                    }
                }

                let x_axis = Arc::new(Axis { name: slice.x.clone(), units: String::new(), values: (0..width).map(|v| v as f64).collect() });
                let y_axis = Arc::new(Axis { name: slice.y.clone(), units: String::new(), values: (0..height).map(|v| v as f64).collect() });
                Ok(Arc::new(Grid { width, height, values, x: x_axis, y: y_axis, units: String::new() }))
            }
        }
    }

    fn make_axis(&self, name: &str, len: usize) -> Arc<Axis> {
        let values = if let Some(v) = self.coords.get(name) {
            if v.len() == len { v.clone() } else { (0..len).map(|i| i as f64).collect() }
        } else {
            (0..len).map(|i| i as f64).collect()
        };
        Arc::new(Axis { name: name.to_string(), units: String::new(), values })
    }

    pub fn series(
        &self,
        variable: &str,
        slice: &Slice,
        dimension: &str,
        x: usize,
        y: usize,
    ) -> Result<Vec<Option<f64>>> {
        let var = self
            .info
            .variables
            .iter()
            .find(|v| v.name == variable)
            .with_context(|| format!("variable {variable:?} not found"))?;
        let dims = &var.dimensions;
        let px = dims.iter().position(|d| d.name == slice.x).context("slice.x not found")?;
        let py = dims.iter().position(|d| d.name == slice.y).context("slice.y not found")?;
        let pd = dims.iter().position(|d| d.name == dimension).with_context(|| format!("dimension {dimension:?} not found"))?;
        ensure!(pd != px && pd != py, "series dimension cannot be a plotted axis");
        let count = dims[pd].len;

        let mut res = Vec::with_capacity(count);
        for step in 0..count {
            let mut s = slice.clone();
            s.indices.insert(dimension.to_string(), step);
            let grid = self.read(variable, &s)?;
            let val = grid.values.get(y * grid.width + x).copied().unwrap_or(f32::NAN);
            res.push(if val.is_finite() { Some(val as f64) } else { None });
        }
        Ok(res)
    }
}

fn extract_numeric_data(data: &NumericData) -> Option<Vec<f64>> {
    match data {
        NumericData::Double { real, imag } if imag.is_none() => Some(real.clone()),
        NumericData::Single { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::Int8 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::UInt8 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::Int16 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::UInt16 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::Int32 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::UInt32 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::Int64 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        NumericData::UInt64 { real, imag } if imag.is_none() => Some(real.iter().map(|&v| v as f64).collect()),
        _ => None,
    }
}

fn guess_dim_name_with_coords(dim_idx: usize, len: usize, total_dims: usize, coords: &HashMap<String, Vec<f64>>) -> String {
    for (name, values) in coords {
        if values.len() == len {
            let lower = name.to_ascii_lowercase();
            if lower.contains("time") || lower == "t" || lower.ends_with("_time") || lower.contains("date") {
                return name.clone();
            }
            if lower.contains("lon") || lower == "x" {
                return name.clone();
            }
            if lower.contains("lat") || lower == "y" {
                return name.clone();
            }
        }
    }
    for (name, values) in coords {
        if values.len() == len {
            return name.clone();
        }
    }
    if total_dims == 3 {
        match dim_idx {
            0 => "time".to_string(),
            1 => "lat".to_string(),
            2 => "lon".to_string(),
            n => format!("dim_{}", n + 1),
        }
    } else if total_dims == 2 {
        match dim_idx {
            0 => "lat".to_string(),
            1 => "lon".to_string(),
            n => format!("dim_{}", n + 1),
        }
    } else {
        format!("dim_{}", dim_idx + 1)
    }
}

fn is_time_dim(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("time") || lower == "t" || lower.ends_with("_time") || lower.contains("date") || lower.contains("step")
}
