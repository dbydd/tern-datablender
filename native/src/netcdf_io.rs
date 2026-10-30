use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, anyhow, bail, ensure};
use netcdf::{
    AttributeValue, Dimension as NcDimension, Extent, Extents, File, Options,
    Variable as NcVariable, types::NcVariableType,
};
use rayon::prelude::*;

use crate::model::{Axis, DatasetInfo, Dimension, Grid, Slice, Variable};

#[derive(Debug)]
pub struct Dataset {
    file: File,
    info: DatasetInfo,
    groups: bool,
}

impl Dataset {
    pub fn open(path: &str) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "NetCDF path is empty");
        let file = netcdf::open(path).with_context(|| format!("cannot open NetCDF file {path}"))?;
        let groups = file.root().is_some();
        let mut variables = Vec::new();
        for var in file.variables() {
            if let Some(variable) = describe(&file, groups, &var, "") {
                variables.push(variable);
            }
        }
        if groups {
            if let Some(root) = file.root() {
                collect_groups(&file, &root, "", &mut variables);
            }
        }
        variables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            file,
            info: DatasetInfo { path: path.to_string(), variables },
            groups,
        })
    }

    pub fn info(&self) -> &DatasetInfo {
        &self.info
    }

    pub fn default_slice(&self, variable: &str) -> Result<Slice> {
        let var = self.variable(variable)?;
        ensure!(is_numeric(&var), "variable {variable:?} is not numeric");
        let dims = var.dimensions();
        ensure!(
            dims.len() >= 2,
            "variable {variable:?} has fewer than two dimensions; it cannot form a plane"
        );
        let mut x = None;
        let mut y = None;
        for (index, dim) in dims.iter().enumerate() {
            match self.role(variable, &var, dim) {
                Some(Role::X) if x.is_none() => x = Some(index),
                Some(Role::Y) if y.is_none() => y = Some(index),
                _ => {}
            }
        }
        let (px, py) = match (x, y) {
            (Some(x), Some(y)) if x != y => (x, y),
            _ => (dims.len() - 1, dims.len() - 2),
        };
        let mut indices = BTreeMap::new();
        for (index, dim) in dims.iter().enumerate() {
            if index != px && index != py {
                indices.insert(dim.name(), 0);
            }
        }
        Ok(Slice { x: dims[px].name(), y: dims[py].name(), indices })
    }

    pub fn read(&self, variable: &str, slice: &Slice) -> Result<Arc<Grid>> {
        let var = self.variable(variable)?;
        ensure!(is_numeric(&var), "variable {variable:?} is not numeric");
        let dims = var.dimensions();
        let (px, py) = axis_positions(dims, slice)?;
        let width = dims[px].len();
        let height = dims[py].len();
        ensure!(
            width > 0 && height > 0,
            "the {:?} x {:?} plane of {variable:?} has no samples",
            slice.x,
            slice.y
        );

        let mut extents = Vec::with_capacity(dims.len());
        for (index, dim) in dims.iter().enumerate() {
            if index == px || index == py {
                extents.push(Extent::SliceCount { start: 0, count: dim.len(), stride: 1 });
            } else {
                let at = slice.indices.get(&dim.name()).copied().unwrap_or(0);
                ensure!(
                    at < dim.len(),
                    "index {at} lies outside dimension {:?} of length {}",
                    dim.name(),
                    dim.len()
                );
                extents.push(Extent::Index(at));
            }
        }
        let raw: Vec<f64> = var
            .get_values(Extents::from(extents))
            .with_context(|| format!("cannot read {variable:?} at the requested slice"))?;
        ensure!(
            raw.len() == width * height,
            "netCDF returned {} samples for a {width} x {height} plane of {variable:?}",
            raw.len()
        );

        let decoder = Decoder::new(&var)?;
        let values = if px < py {
            let mut transposed = vec![f32::NAN; width * height];
            transposed.par_chunks_mut(width).enumerate().for_each(|(row, cells)| {
                for (column, cell) in cells.iter_mut().enumerate() {
                    *cell = decoder.decode(raw[column * height + row]);
                }
            });
            transposed
        } else {
            raw.par_iter().map(|&value| decoder.decode(value)).collect()
        };

        let x = self.axis(variable, &var, &dims[px])?;
        let y = self.axis(variable, &var, &dims[py])?;
        let units = attribute_string(&var, "units").unwrap_or_default();
        Ok(Arc::new(Grid { width, height, values, x, y, units }))
    }

    pub fn series(
        &self,
        variable: &str,
        slice: &Slice,
        dimension: &str,
        x: usize,
        y: usize,
    ) -> Result<Vec<Option<f64>>> {
        let var = self.variable(variable)?;
        ensure!(is_numeric(&var), "variable {variable:?} is not numeric");
        let dims = var.dimensions();
        let (px, py) = axis_positions(dims, slice)?;
        let width = dims[px].len();
        let height = dims[py].len();
        ensure!(
            x < width && y < height,
            "point ({x}, {y}) lies outside the {width} x {height} plane of {variable:?}"
        );
        ensure!(!dimension.is_empty(), "series dimension is empty");
        let Some(pd) = dims.iter().position(|dim| dim.name() == dimension) else {
            bail!("{dimension:?} is not a dimension of {variable:?}");
        };
        ensure!(
            pd != px && pd != py,
            "series dimension {dimension:?} is one of the plotted axes; it cannot vary along the plane"
        );
        let count = dims[pd].len();
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut extents = Vec::with_capacity(dims.len());
        for (index, dim) in dims.iter().enumerate() {
            if index == px {
                extents.push(Extent::Index(x));
            } else if index == py {
                extents.push(Extent::Index(y));
            } else if index == pd {
                extents.push(Extent::SliceCount { start: 0, count, stride: 1 });
            } else {
                let at = slice.indices.get(&dim.name()).copied().unwrap_or(0);
                ensure!(
                    at < dim.len(),
                    "index {at} lies outside dimension {:?} of length {}",
                    dim.name(),
                    dim.len()
                );
                extents.push(Extent::Index(at));
            }
        }
        let raw: Vec<f64> = var
            .get_values(Extents::from(extents))
            .with_context(|| format!("cannot read {dimension:?} series of {variable:?} at ({x}, {y})"))?;
        ensure!(
            raw.len() == count,
            "netCDF returned {} samples for a {count}-step series of {variable:?}",
            raw.len()
        );
        let decoder = Decoder::new(&var)?;
        Ok(raw.into_iter().map(|value| decoder.value(value)).collect())
    }

    fn variable(&self, name: &str) -> Result<NcVariable<'_>> {
        ensure!(!name.trim().is_empty(), "NetCDF variable name is empty");
        if name.contains('/') && !self.groups {
            bail!("nested variable path {name:?} needs a netCDF-4 file");
        }
        self.file
            .variable(name)
            .ok_or_else(|| anyhow!("variable {name:?} is not in this dataset"))
    }

    fn axis(&self, var_path: &str, var: &NcVariable<'_>, dim: &NcDimension<'_>) -> Result<Arc<Axis>> {
        let len = dim.len();
        if let Some(coord) = coordinate_variable(&self.file, self.groups, var, var_path, dim) {
            let raw: Vec<f64> = coord
                .get_values(..)
                .with_context(|| format!("cannot read coordinate {:?}", coord.name()))?;
            ensure!(
                raw.len() == len,
                "coordinate {:?} has {} samples but dimension {:?} has {len}",
                coord.name(),
                raw.len(),
                dim.name()
            );
            let decoder = Decoder::new(&coord)?;
            let units = attribute_string(&coord, "units").unwrap_or_default();
            let values = raw.into_iter().map(|value| decoder.scaled(value)).collect();
            return Ok(Arc::new(Axis { name: coord.name(), units, values }));
        }
        Ok(Arc::new(Axis {
            name: dim.name(),
            units: String::new(),
            values: (0..len).map(|index| index as f64).collect(),
        }))
    }

    fn role(&self, var_path: &str, var: &NcVariable<'_>, dim: &NcDimension<'_>) -> Option<Role> {
        if let Some(coord) = coordinate_variable(&self.file, self.groups, var, var_path, dim) {
            if let Some(role) = coordinate_role(&coord) {
                return Some(role);
            }
        }
        dimension_role(&dim.name())
    }
}

pub fn write_grid(path: &str, grid: &Grid) -> Result<()> {
    ensure!(grid.width > 0 && grid.height > 0, "cannot write an empty grid");
    ensure!(
        grid.values.len() == grid.width * grid.height,
        "grid holds {} values but claims a {} x {} shape",
        grid.values.len(),
        grid.width,
        grid.height
    );
    ensure!(
        grid.x.values.len() == grid.width,
        "grid has {} x coordinates for {} columns",
        grid.x.values.len(),
        grid.width
    );
    ensure!(
        grid.y.values.len() == grid.height,
        "grid has {} y coordinates for {} rows",
        grid.y.values.len(),
        grid.height
    );

    let x_name = dimension_name(&grid.x.name, "x");
    let mut y_name = dimension_name(&grid.y.name, "y");
    if y_name == x_name {
        y_name.push_str("_y");
    }
    ensure!(
        x_name != "result" && y_name != "result",
        "grid axes cannot be named \"result\""
    );

    let mut file = netcdf::create_with(path, Options::NETCDF4 | Options::NOCLOBBER)
        .with_context(|| format!("cannot create {path}; existing files are never overwritten"))?;
    file.add_dimension(&x_name, grid.width)
        .with_context(|| format!("cannot define dimension {x_name:?}"))?;
    file.add_dimension(&y_name, grid.height)
        .with_context(|| format!("cannot define dimension {y_name:?}"))?;

    {
        let mut x = file
            .add_variable::<f64>(&x_name, &[x_name.as_str()])
            .with_context(|| format!("cannot define coordinate {x_name:?}"))?;
        if !grid.x.units.is_empty() {
            x.put_attribute("units", grid.x.units.as_str())?;
        }
        x.put_values(&grid.x.values, ..)
            .with_context(|| format!("cannot write coordinate {x_name:?}"))?;
    }
    {
        let mut y = file
            .add_variable::<f64>(&y_name, &[y_name.as_str()])
            .with_context(|| format!("cannot define coordinate {y_name:?}"))?;
        if !grid.y.units.is_empty() {
            y.put_attribute("units", grid.y.units.as_str())?;
        }
        y.put_values(&grid.y.values, ..)
            .with_context(|| format!("cannot write coordinate {y_name:?}"))?;
    }
    {
        let mut result = file
            .add_variable::<f32>("result", &[y_name.as_str(), x_name.as_str()])
            .with_context(|| format!("cannot define result variable in {path}"))?;
        if !grid.units.is_empty() {
            result.put_attribute("units", grid.units.as_str())?;
        }
        result.put_attribute("_FillValue", f32::NAN)?;
        result
            .put_values(&grid.values, (.., ..))
            .with_context(|| format!("cannot write result variable in {path}"))?;
    }
    file.close().with_context(|| format!("cannot finish writing {path}"))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    X,
    Y,
    Z,
    Time,
}

struct Decoder {
    scale: f64,
    offset: f64,
    masks: Vec<f64>,
}

impl Decoder {
    fn new(var: &NcVariable<'_>) -> Result<Self> {
        let scale = attribute_numbers(var, "scale_factor").first().copied().unwrap_or(1.0);
        let offset = attribute_numbers(var, "add_offset").first().copied().unwrap_or(0.0);
        ensure!(scale.is_finite(), "scale_factor of {:?} is not finite", var.name());
        ensure!(offset.is_finite(), "add_offset of {:?} is not finite", var.name());
        let mut masks = attribute_numbers(var, "_FillValue");
        masks.extend(attribute_numbers(var, "missing_value"));
        Ok(Self { scale, offset, masks })
    }

    fn masked(&self, raw: f64) -> bool {
        !raw.is_finite() || self.masks.iter().any(|mask| mask.is_finite() && *mask == raw)
    }

    fn scaled(&self, raw: f64) -> f64 {
        if self.masked(raw) {
            return f64::NAN;
        }
        let value = raw * self.scale + self.offset;
        if value.is_finite() { value } else { f64::NAN }
    }

    fn decode(&self, raw: f64) -> f32 {
        self.scaled(raw) as f32
    }

    fn value(&self, raw: f64) -> Option<f64> {
        let value = self.scaled(raw);
        value.is_finite().then_some(value)
    }
}

fn collect_groups(
    file: &File,
    group: &netcdf::Group<'_>,
    prefix: &str,
    out: &mut Vec<Variable>,
) {
    for child in group.groups() {
        let name = child.name();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        for var in child.variables() {
            if let Some(variable) = describe(file, true, &var, &path) {
                out.push(variable);
            }
        }
        collect_groups(file, &child, &path, out);
    }
}

fn describe(
    file: &File,
    groups: bool,
    var: &NcVariable<'_>,
    prefix: &str,
) -> Option<Variable> {
    if !is_numeric(var) {
        return None;
    }
    let dims = var.dimensions();
    if dims.len() < 2 {
        return None;
    }
    let name = if prefix.is_empty() { var.name() } else { format!("{prefix}/{}", var.name()) };
    let mut dimensions = Vec::with_capacity(dims.len());
    for dim in dims {
        let units = coordinate_variable(file, groups, var, &name, dim)
            .and_then(|coord| attribute_string(&coord, "units"))
            .unwrap_or_default();
        dimensions.push(Dimension { name: dim.name(), len: dim.len(), units });
    }
    let units = attribute_string(var, "units").unwrap_or_default();
    let long_name = attribute_string(var, "long_name")
        .or_else(|| attribute_string(var, "standard_name"))
        .unwrap_or_default();
    Some(Variable { name, dimensions, units, long_name })
}

fn coordinate_variable<'f>(
    file: &'f File,
    groups: bool,
    var: &NcVariable<'_>,
    var_path: &str,
    dim: &NcDimension<'_>,
) -> Option<NcVariable<'f>> {
    let dim_name = dim.name();
    let mut candidates = vec![dim_name.clone()];
    if let Some(list) = attribute_string(var, "coordinates") {
        for name in list.split_whitespace() {
            if !candidates.iter().any(|known| known == name) {
                candidates.push(name.to_string());
            }
        }
    }
    for name in candidates {
        let Some(coord) = find_in_group(file, groups, var_path, &name) else {
            continue;
        };
        if !is_numeric(&coord) {
            continue;
        }
        let dims = coord.dimensions();
        if dims.len() != 1 || dims[0].name() != dim_name || dims[0].len() != dim.len() {
            continue;
        }
        return Some(coord);
    }
    None
}

fn find_in_group<'f>(
    file: &'f File,
    groups: bool,
    var_path: &str,
    name: &str,
) -> Option<NcVariable<'f>> {
    if name.is_empty() {
        return None;
    }
    if groups {
        if let Some((parent, _)) = var_path.rsplit_once('/') {
            if !parent.is_empty() {
                if let Some(var) = file.variable(&format!("{parent}/{name}")) {
                    return Some(var);
                }
            }
        }
        if name.contains('/') {
            return file.variable(name);
        }
    }
    file.variable(name)
}

fn axis_positions(dims: &[NcDimension<'_>], slice: &Slice) -> Result<(usize, usize)> {
    ensure!(!slice.x.is_empty() && !slice.y.is_empty(), "slice must name both an x and a y dimension");
    ensure!(slice.x != slice.y, "slice x and y must name different dimensions");
    let px = position(dims, &slice.x)?;
    let py = position(dims, &slice.y)?;
    for (name, at) in &slice.indices {
        if *name == slice.x || *name == slice.y {
            bail!(
                "slice indices must not include the plotted axes {:?} and {:?}",
                slice.x,
                slice.y
            );
        }
        let index = position(dims, name)?;
        let len = dims[index].len();
        ensure!(*at < len, "index {at} lies outside dimension {name:?} of length {len}");
    }
    Ok((px, py))
}

fn position(dims: &[NcDimension<'_>], name: &str) -> Result<usize> {
    dims.iter()
        .position(|dim| dim.name() == name)
        .ok_or_else(|| anyhow!("{name:?} is not a dimension of this variable"))
}

fn is_numeric(var: &NcVariable<'_>) -> bool {
    matches!(var.vartype(), NcVariableType::Int(_) | NcVariableType::Float(_))
}

fn attribute_string(var: &NcVariable<'_>, name: &str) -> Option<String> {
    match var.attribute_value(name)? {
        Ok(value) => String::try_from(value).ok(),
        Err(_) => None,
    }
}

fn attribute_numbers(var: &NcVariable<'_>, name: &str) -> Vec<f64> {
    let Some(Ok(value)) = var.attribute_value(name) else {
        return Vec::new();
    };
    match value {
        AttributeValue::Uchar(v) => vec![f64::from(v)],
        AttributeValue::Uchars(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Schar(v) => vec![f64::from(v)],
        AttributeValue::Schars(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Ushort(v) => vec![f64::from(v)],
        AttributeValue::Ushorts(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Short(v) => vec![f64::from(v)],
        AttributeValue::Shorts(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Uint(v) => vec![f64::from(v)],
        AttributeValue::Uints(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Int(v) => vec![f64::from(v)],
        AttributeValue::Ints(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Ulonglong(v) => vec![v as f64],
        AttributeValue::Ulonglongs(v) => v.into_iter().map(|value| value as f64).collect(),
        AttributeValue::Longlong(v) => vec![v as f64],
        AttributeValue::Longlongs(v) => v.into_iter().map(|value| value as f64).collect(),
        AttributeValue::Float(v) => vec![f64::from(v)],
        AttributeValue::Floats(v) => v.into_iter().map(f64::from).collect(),
        AttributeValue::Double(v) => vec![v],
        AttributeValue::Doubles(v) => v,
        AttributeValue::Str(_) | AttributeValue::Strs(_) => Vec::new(),
    }
}

fn coordinate_role(coord: &NcVariable<'_>) -> Option<Role> {
    if let Some(axis) = attribute_string(coord, "axis") {
        match axis.trim().to_ascii_uppercase().as_str() {
            "X" => return Some(Role::X),
            "Y" => return Some(Role::Y),
            "Z" => return Some(Role::Z),
            "T" => return Some(Role::Time),
            _ => {}
        }
    }
    if attribute_string(coord, "positive").is_some() {
        return Some(Role::Z);
    }
    if let Some(name) = attribute_string(coord, "standard_name") {
        if let Some(role) = standard_name_role(&name) {
            return Some(role);
        }
    }
    attribute_string(coord, "units").and_then(|units| units_role(&units))
}

fn standard_name_role(name: &str) -> Option<Role> {
    let name = name.trim().to_ascii_lowercase();
    if name.contains("longitude") || name.contains("projection_x") || name.contains("grid_longitude") {
        Some(Role::X)
    } else if name.contains("latitude") || name.contains("projection_y") || name.contains("grid_latitude") {
        Some(Role::Y)
    } else if name.contains("time") {
        Some(Role::Time)
    } else if name.contains("depth")
        || name.contains("height")
        || name.contains("altitude")
        || name.contains("pressure")
        || name.contains("level")
    {
        Some(Role::Z)
    } else {
        None
    }
}

fn units_role(units: &str) -> Option<Role> {
    let units = units.trim().to_ascii_lowercase();
    if units.is_empty() {
        return None;
    }
    if units.contains("degree_east") || units.contains("degrees_east") {
        Some(Role::X)
    } else if units.contains("degree_north") || units.contains("degrees_north") {
        Some(Role::Y)
    } else if units.contains(" since ") {
        Some(Role::Time)
    } else if units.contains("sigma_level")
        || units.contains("layer")
        || units.contains("pressure")
        || units.starts_with("pa")
        || units.starts_with("hpa")
    {
        Some(Role::Z)
    } else {
        None
    }
}

fn dimension_role(name: &str) -> Option<Role> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    if name.contains("lon") || name == "x" || name == "xi" || name.contains("easting") || name == "xcoord" {
        Some(Role::X)
    } else if name.contains("lat") || name == "y" || name == "eta" || name.contains("northing") || name == "ycoord" {
        Some(Role::Y)
    } else if name == "time" || name == "t" || name.ends_with("_time") || name.contains("date") {
        Some(Role::Time)
    } else if name == "z"
        || name == "lev"
        || name == "level"
        || name.contains("depth")
        || name.contains("height")
        || name == "alt"
        || name.contains("pressure")
        || name.contains("sigma")
    {
        Some(Role::Z)
    } else {
        None
    }
}

fn dimension_name(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|ch| if ch == '/' || ch.is_control() || ch.is_whitespace() { '_' } else { ch })
        .collect();
    if cleaned.is_empty() { fallback.to_string() } else { cleaned }
}
