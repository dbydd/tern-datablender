use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use anyhow::{bail, ensure, Context, Result};
use hdf5_metno::types::{TypeDescriptor, VarLenAscii, VarLenUnicode};
use hdf5_metno::{Dataset as H5Dataset, File as H5File, Group as H5Group, Hyperslab, Selection, SliceOrIndex};

use crate::model::{Axis, DatasetInfo, Dimension, Grid, Slice, Variable};

#[derive(Debug)]
pub struct Hdf5Dataset {
    path: String,
    info: DatasetInfo,
    coords: HashMap<String, Vec<f64>>,
}

impl Hdf5Dataset {
    pub fn open(path: &str) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "HDF5 path is empty");
        let file = H5File::open(path).with_context(|| format!("cannot open HDF5 file {path}"))?;

        let mut coords = HashMap::new();
        let mut variables = Vec::new();

        collect_group(&file.as_group()?, "", &mut coords, &mut variables)?;

        variables.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self {
            path: path.to_string(),
            info: DatasetInfo {
                path: path.to_string(),
                variables,
            },
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

        // Prefer spatial dimensions (e.g. named x/lon, y/lat)
        let mut x = None;
        let mut y = None;
        for (i, d) in dims.iter().enumerate() {
            let lower = d.name.to_ascii_lowercase();
            if (lower.contains("lon") || lower == "x" || lower.contains("width") || lower == "w") && x.is_none() {
                x = Some(i);
            } else if (lower.contains("lat") || lower == "y" || lower.contains("height") || lower == "h") && y.is_none() {
                y = Some(i);
            }
        }

        let (px, py) = match (x, y) {
            (Some(px), Some(py)) if px != py => (px, py),
            _ => {
                // If not matched by name, find candidate 2D spatial dimensions where len > 1
                let big_dims: Vec<usize> = dims
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| d.len > 1 && !is_time_dim(&d.name))
                    .map(|(i, _)| i)
                    .collect();

                if big_dims.len() >= 2 {
                    // In C-order [..., Y, X], the last two big dimensions are typically (H, W) -> X is last, Y is second to last
                    let last = big_dims[big_dims.len() - 1];
                    let prev = big_dims[big_dims.len() - 2];
                    (last, prev)
                } else if dims.len() >= 2 {
                    // Fall back to the last two dimensions: px = last (X/width), py = second to last (Y/height)
                    (dims.len() - 1, dims.len() - 2)
                } else {
                    bail!("variable {variable:?} does not have enough dimensions for a plane");
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

        let mut indices = slice.indices.clone();
        if steps != 0 {
            let time_dim = dims
                .iter()
                .find(|d| is_time_dim(&d.name))
                .with_context(|| format!("Time shift requires a time dimension in {variable:?}"))?;
            ensure!(
                time_dim.name != slice.x && time_dim.name != slice.y,
                "Time dimension cannot be a plotted axis"
            );
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
                    units: var.units.clone(),
                }));
            }
            indices.insert(time_dim.name.clone(), usize::try_from(shifted)?);
        }

        let file = H5File::open(&self.path)?;
        let dset = file.dataset(variable)?;
        let ndim = dset.ndim();
        ensure!(ndim == dims.len(), "dataset dimension count mismatch");

        let mut slices: Vec<SliceOrIndex> = Vec::with_capacity(ndim);
        for (i, d) in dims.iter().enumerate() {
            if i == px || i == py {
                slices.push((..).into());
            } else {
                let at = indices.get(&d.name).copied().unwrap_or(0).min(d.len.saturating_sub(1));
                slices.push(SliceOrIndex::Index(at));
            }
        }

        let hyper: Hyperslab = slices.into();
        let selection: Selection = hyper.into();
        let raw_arr = dset.read_slice::<f64, _, ndarray::IxDyn>(selection)?;
        let raw_slice = raw_arr.as_slice().context("array is not contiguous")?;

        let mut values = vec![0.0f32; width * height];
        if py < px {
            // Row-major order in HDF5: py comes before px, so array layout is [height, width]
            for (i, &v) in raw_slice.iter().enumerate().take(width * height) {
                values[i] = v as f32;
            }
        } else {
            // Array layout in HDF5 is [width, height], transpose to [height, width]
            for x in 0..width {
                for y in 0..height {
                    let v = raw_slice[x * height + y];
                    values[y * width + x] = v as f32;
                }
            }
        }

        let x_axis = self.make_axis(&slice.x, width);
        let y_axis = self.make_axis(&slice.y, height);
        Ok(Arc::new(Grid {
            width,
            height,
            values,
            x: x_axis,
            y: y_axis,
            units: var.units.clone(),
        }))
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
        let pd = dims
            .iter()
            .position(|d| d.name == dimension)
            .with_context(|| format!("dimension {dimension:?} not found"))?;
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

    fn make_axis(&self, name: &str, len: usize) -> Arc<Axis> {
        let values = if let Some(v) = self.coords.get(name) {
            if v.len() == len {
                v.clone()
            } else {
                (0..len).map(|i| i as f64).collect()
            }
        } else {
            (0..len).map(|i| i as f64).collect()
        };
        Arc::new(Axis {
            name: name.to_string(),
            units: String::new(),
            values,
        })
    }
}

fn collect_group(
    group: &H5Group,
    prefix: &str,
    coords: &mut HashMap<String, Vec<f64>>,
    variables: &mut Vec<Variable>,
) -> Result<()> {
    for name in group.member_names()? {
        if name.starts_with('#') {
            continue;
        }
        let full_name = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        if let Ok(dset) = group.dataset(&name) {
            // Check 1D coordinate array
            let shape = dset.shape();
            let len = if shape.len() == 1 {
                shape[0]
            } else if shape.len() == 2 && (shape[0] == 1 || shape[1] == 1) {
                shape[0].max(shape[1])
            } else {
                0
            };
            if len > 1 {
                if let Ok(arr) = dset.read_1d::<f64>() {
                    coords.insert(full_name.clone(), arr.to_vec());
                    coords.insert(name.clone(), arr.to_vec());
                }
            }

            // Exclude datasets with fewer than 2 dimensions or total elements 0
            if shape.len() < 2
                || shape.iter().any(|&d| d == 0)
                || (shape.len() == 2 && (shape[0] == 1 || shape[1] == 1))
            {
                continue;
            }

            let Ok(dtype) = dset.dtype() else { continue };
            let Ok(desc) = dtype.to_descriptor() else { continue };
            if matches!(desc, TypeDescriptor::Compound(_)) {
                continue;
            }

            let mut dimensions = Vec::with_capacity(shape.len());
            for (i, &dim_len) in shape.iter().enumerate() {
                let dim_name = guess_dim_name(i, dim_len, shape.len(), coords, &dset);
                dimensions.push(Dimension {
                    name: dim_name,
                    len: dim_len,
                    units: String::new(),
                });
            }

            let units = read_string_attr(&dset, "units").unwrap_or_default();
            let long_name = read_string_attr(&dset, "long_name")
                .or_else(|| read_string_attr(&dset, "standard_name"))
                .unwrap_or_else(|| full_name.clone());

            variables.push(Variable {
                name: full_name,
                dimensions,
                units,
                long_name,
            });
        } else if let Ok(sub_group) = group.group(&name) {
            let _ = collect_group(&sub_group, &full_name, coords, variables);
        }
    }
    Ok(())
}

fn read_string_attr(dset: &H5Dataset, attr_name: &str) -> Option<String> {
    if let Ok(attr) = dset.attr(attr_name) {
        if let Ok(val) = attr.read_scalar::<VarLenUnicode>() {
            return Some(val.to_string());
        }
        if let Ok(val) = attr.read_scalar::<VarLenAscii>() {
            return Some(val.to_string());
        }
        if let Ok(val) = attr.read_1d::<u8>() {
            return String::from_utf8(val.to_vec()).ok();
        }
    }
    None
}

fn guess_dim_name(
    dim_idx: usize,
    len: usize,
    total_dims: usize,
    coords: &HashMap<String, Vec<f64>>,
    _dset: &H5Dataset,
) -> String {
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

    // Standard C-order heuristics for multidimensional scientific/ML arrays:
    // 4D: e.g. [sample/time/batch, height/lat, width/lon, channel]
    if total_dims == 4 {
        match dim_idx {
            0 => "sample".to_string(),
            1 => "lat".to_string(),
            2 => "lon".to_string(),
            3 => "channel".to_string(),
            n => format!("dim_{}", n + 1),
        }
    } else if total_dims == 3 {
        // [time/batch, height/lat, width/lon]
        match dim_idx {
            0 => "time".to_string(),
            1 => "lat".to_string(),
            2 => "lon".to_string(),
            n => format!("dim_{}", n + 1),
        }
    } else if total_dims == 2 {
        // [height/lat, width/lon]
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
    lower.contains("time")
        || lower == "t"
        || lower.ends_with("_time")
        || lower.contains("date")
        || lower.contains("step")
        || lower.contains("sample")
        || lower.contains("batch")
}
