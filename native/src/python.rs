use anyhow::{Context, Result};
use crate::model::{ColorMap, Project, ScalarNode, VectorNode};

pub fn export(project: &Project) -> Result<String> {
    let graph = project.graph.as_ref().context("This project has no node graph")?;
    let scene = graph.compile()?;
    let mut text = String::from(r#"#!/usr/bin/env python3
"""Exported NetCDF workflow. Requires numpy, xarray, scipy, netCDF4 and matplotlib."""
import argparse
import json
from pathlib import Path
import numpy as np
import xarray as xr
from scipy.ndimage import convolve, gaussian_filter
import matplotlib.pyplot as plt
from matplotlib.colors import LinearSegmentedColormap

parser = argparse.ArgumentParser(description=__doc__)
"#);
    text.push_str(&format!("parser.add_argument('--source', default={})\nparser.add_argument('--output', default='netcdf-result.png')\nargs = parser.parse_args()\n", quote(&project.path)));
    text.push_str("if Path(args.output).exists():\n    raise FileExistsError(args.output)\n");
    text.push_str(&format!("slice_spec = json.loads({})\n", quote(&serde_json::to_string(&project.slice)?)));
    text.push_str(r#"
def field(name):
    group, _, variable = name.rpartition('/')
    with xr.open_dataset(args.source, group=group or None) as dataset:
        data = dataset[variable].load()
    dims = list(data.dims)
    def axis(kind, fallback):
        explicit = slice_spec.get(kind)
        if explicit:
            return explicit
        expected = 'X' if kind == 'x' else 'Y'
        standards = ('longitude', 'projection_x_coordinate') if kind == 'x' else ('latitude', 'projection_y_coordinate')
        names = ('lon', 'longitude', 'x') if kind == 'x' else ('lat', 'latitude', 'y')
        for dim in dims:
            coord = data.coords.get(dim)
            attrs = coord.attrs if coord is not None else {}
            if attrs.get('axis', '').upper() == expected or attrs.get('standard_name') in standards or dim.lower() in names:
                return dim
        return fallback
    x = axis('x', dims[-1])
    y = axis('y', dims[-2])
    indices = slice_spec.get('indices', {})
    data = data.isel({d: indices.get(d, 0) for d in dims if d not in (x, y)}).transpose(y, x)
    for dim in (y, x):
        if dim not in data.coords:
            data = data.assign_coords({dim: np.arange(data.sizes[dim], dtype=float)})
    return data.astype(np.float32)


def same_grid(a, b):
    if a.shape != b.shape:
        raise ValueError('Fields have different coordinates')
    for ad, bd in zip(a.dims, b.dims):
        if not np.array_equal(a[ad].values, b[bd].values) or a[ad].attrs.get('units', '') != b[bd].attrs.get('units', ''):
            raise ValueError('Fields have different coordinates')
    if a.attrs.get('units', '') != b.attrs.get('units', ''):
        raise ValueError('Fields require matching units')


def gaussian(a, sigma):
    valid = np.isfinite(a.values)
    options = dict(sigma=sigma, radius=int(np.ceil(3 * sigma)), mode='mirror')
    weights = gaussian_filter(valid.astype(float), **options)
    values = gaussian_filter(np.where(valid, a.values, 0).astype(float), **options)
    with np.errstate(invalid='ignore', divide='ignore'):
        values = np.where(valid & (weights > 0), values / weights, np.nan)
    return a.copy(data=values.astype(np.float32))


def convolution(a, kernel):
    values = convolve(a.values.astype(float), kernel, mode='mirror')
    missing = convolve((~np.isfinite(a.values)).astype(float), (kernel != 0).astype(float), mode='mirror')
    return a.copy(data=np.where(np.isfinite(a.values) & (missing == 0), values, np.nan).astype(np.float32))


def difference(a, b):
    same_grid(a, b)
    return a.copy(data=(a.values - b.values).astype(np.float32))


def scale(a, factor, offset):
    return a.copy(data=(a.values.astype(float) * factor + offset).astype(np.float32))


def vector(u, v):
    same_grid(u, v)
    return u, v


def gradient(a):
    y, x = a.dims
    if min(a.shape) < 2:
        raise ValueError('Gradient requires two samples on each axis')
    for dim in (y, x):
        coords = a[dim].values
        steps = np.diff(coords)
        if not np.all(np.isfinite(coords)) or not (np.all(steps > 0) or np.all(steps < 0)):
            raise ValueError('Gradient requires strictly monotonic coordinates')
    dy, dx = np.gradient(a.values.astype(float), a[y].values, a[x].values, edge_order=1)
    components = []
    for values, dim in ((dx, x), (dy, y)):
        data = a.copy(data=np.where(np.isfinite(a.values), values, np.nan).astype(np.float32))
        denominator = a[dim].attrs.get('units', '')
        denominator = 'degree' if denominator.startswith('degrees_') else denominator or 'index'
        unit = a.attrs.get('units', '')
        data.attrs['units'] = f'{unit} / {denominator}' if unit else f'1 / {denominator}'
        components.append(data)
    return tuple(components)


def magnitude(v):
    u, w = v
    same_grid(u, w)
    return u.copy(data=np.hypot(u.values, w.values).astype(np.float32))


fig, ax = plt.subplots(figsize=(10, 8), constrained_layout=True)
ax.set_facecolor((0, 0, 0, 0))
"#);
    let mut emitter = Emitter { lines: String::new(), scalars: Vec::new(), vectors: Vec::new() };
    let mut layers = Vec::new();
    for layer in &scene.layers {
        let name = emitter.scalar(&layer.field)?;
        layers.push((name, layer));
    }
    let mut arrows = Vec::new();
    for arrow in &scene.arrows { arrows.push((emitter.vector(&arrow.field)?, arrow)); }
    text.push_str(&emitter.lines);
    for (i, (name, layer)) in layers.iter().enumerate() {
        let map = match layer.colormap { ColorMap::Viridis => "viridis", ColorMap::Thermal => "inferno", ColorMap::Diverging => "RdBu_r", ColorMap::Gray => "gray" };
        let bounds = match layer.range { Some([lo,hi]) => format!(", vmin={lo:?}, vmax={hi:?}"), None => String::new() };
        text.push_str(&format!("\nvalues = {name}.values\ny, x = {name}.dims\nextent = [float({name}[x].values.min()), float({name}[x].values.max()), float({name}[y].values.min()), float({name}[y].values.max())]\nif {name}[x].values[0] > {name}[x].values[-1]:\n    values = values[:, ::-1]\nif {name}[y].values[0] > {name}[y].values[-1]:\n    values = values[::-1]\nimage_{i} = ax.imshow(values, origin='lower', extent=extent, aspect='auto', interpolation='nearest', cmap={map:?}, alpha={alpha:?}{bounds})\n", alpha=layer.opacity));
        if layers.len() == 1 { text.push_str(&format!("fig.colorbar(image_{i}, ax=ax, label={name}.attrs.get('units', ''))\n")); }
    }
    for (name, spec) in arrows {
        text.push_str(&format!("\nu, v = {name}\ny, x = u.dims\nstep = {stride}\nus, vs = u.values[::step, ::step], v.values[::step, ::step]\nlength = np.hypot(us, vs)\nmaximum = np.nanmax(length)\nif np.isfinite(maximum) and maximum > 0:\n    ax.quiver(u[x].values[::step], u[y].values[::step], us, vs, angles='xy', scale_units='width', scale=maximum * max(u.shape) / (step * {scale:?}), color='white')\n",stride=spec.stride,scale=spec.scale));
    }
    text.push_str("ax.set_xlabel(x)\nax.set_ylabel(y)\nfig.savefig(args.output, dpi=160, transparent=True)\nplt.close(fig)\n");
    Ok(text)
}

fn quote(s: &str) -> String { serde_json::to_string(s).unwrap() }

struct Emitter { lines: String, scalars: Vec<(String, String)>, vectors: Vec<(String, String)> }
impl Emitter {
    fn scalar(&mut self, n: &ScalarNode) -> Result<String> {
        let key = serde_json::to_string(n)?;
        if let Some((_, name)) = self.scalars.iter().find(|(k,_)| *k == key) { return Ok(name.clone()); }
        let expression = match n {
            ScalarNode::Source { variable } => format!("field({})", quote(variable)),
            ScalarNode::Gaussian { input,sigma } => format!("gaussian({}, {sigma:?})",self.scalar(input)?),
            ScalarNode::Convolve { input,kernel,side } => format!("convolution({}, np.array({kernel:?}).reshape({side}, {side}))",self.scalar(input)?),
            ScalarNode::Difference { left,right } => format!("difference({}, {})",self.scalar(left)?,self.scalar(right)?),
            ScalarNode::Scale { input,factor,offset } => format!("scale({}, {factor:?}, {offset:?})",self.scalar(input)?),
            ScalarNode::Magnitude { input } => format!("magnitude({})",self.vector(input)?),
        };
        let name = format!("field_{}",self.scalars.len()+1);
        self.lines.push_str(&format!("{name} = {expression}\n"));
        self.scalars.push((key,name.clone()));
        Ok(name)
    }
    fn vector(&mut self,n:&VectorNode)->Result<String> {
        let key=serde_json::to_string(n)?;
        if let Some((_,name))=self.vectors.iter().find(|(k,_)|*k==key) { return Ok(name.clone()); }
        let expression=match n {
            VectorNode::Components {u,v}=>format!("vector({}, {})",self.scalar(u)?,self.scalar(v)?),
            VectorNode::Gradient {input}=>format!("gradient({})",self.scalar(input)?),
        };
        let name=format!("vector_{}",self.vectors.len()+1);
        self.lines.push_str(&format!("{name} = {expression}\n"));
        self.vectors.push((key,name.clone()));
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    #[test]
    fn test_python_export_structure() -> Result<()> {
        let mut graph = NodeGraph::source("uo");
        graph.nodes.push(GraphNode {
            id: 2,
            name: "vo".into(),
            position: [240.0, 40.0],
            inputs: vec![],
            operation: Operation::Source { variable: "vo".into() },
        });
        graph.nodes.push(GraphNode {
            id: 3,
            name: "Gaussian".into(),
            position: [40.0, 200.0],
            inputs: vec![Some(1)],
            operation: Operation::Gaussian { sigma: 1.5 },
        });
        graph.layers.push(GraphLayer {
            node: 2,
            visible: true,
            colormap: ColorMap::Thermal,
            opacity: 0.6,
            range: None,
            stride: 8,
            arrow_scale: 1.0,
        });
        let project = Project {
            path: "data/sample_ocean.nc".into(),
            graph: Some(graph),
            ..Project::default()
        };
        let script = export(&project)?;
        assert!(script.contains("field(\"uo\")"));
        assert!(script.contains("field(\"vo\")"));
        assert!(script.contains("gaussian("));
        assert!(script.contains("viridis"));
        assert!(script.contains("inferno"));
        assert!(script.contains("alpha=0.6"));
        Ok(())
    }
}
