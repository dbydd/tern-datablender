//! In-memory raster rendering: scalar layers, vector arrows, PNG bytes.
//!
//! Everything happens in this process. Layers are painted into one RGBA buffer
//! with Rayon over output rows, arrows are stamped on top, and the buffer is
//! encoded once with the fast PNG compressor. No plotting library, no child
//! process, no intermediate file.

use std::{
    fs::OpenOptions,
    io::{ErrorKind, Write},
    sync::{Arc, LazyLock},
};

use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;

use crate::model::{Arrows, Axis, ColorMap, Grid, Layer, Rendered, VectorGrid};

/// Longest output edge in pixels. Larger grids are sampled down; smaller grids
/// keep their native resolution so the view scales crisply in the browser.
const MAX_EDGE: usize = 1536;

const LUT_LEN: usize = 256;
const ANCHORS: usize = 33;

/// Viridis, sampled every eighth entry of the reference 256-step table.
const VIRIDIS: [[u8; 3]; ANCHORS] = [
    [68, 1, 84],
    [71, 13, 96],
    [72, 24, 106],
    [72, 35, 116],
    [71, 45, 123],
    [69, 55, 129],
    [66, 64, 134],
    [62, 73, 137],
    [59, 82, 139],
    [55, 91, 141],
    [51, 99, 141],
    [47, 107, 142],
    [44, 114, 142],
    [41, 122, 142],
    [38, 130, 142],
    [35, 137, 142],
    [33, 145, 140],
    [31, 152, 139],
    [31, 160, 136],
    [34, 167, 133],
    [40, 174, 128],
    [50, 182, 122],
    [63, 188, 115],
    [78, 195, 107],
    [94, 201, 98],
    [112, 207, 87],
    [132, 212, 75],
    [152, 216, 62],
    [173, 220, 48],
    [194, 223, 35],
    [216, 226, 25],
    [236, 229, 27],
    [253, 231, 37],
];

/// Inferno, sampled every eighth entry of the reference table.
const THERMAL: [[u8; 3]; ANCHORS] = [
    [0, 0, 4],
    [4, 3, 18],
    [11, 7, 36],
    [21, 11, 55],
    [33, 12, 74],
    [47, 10, 91],
    [61, 9, 101],
    [74, 12, 107],
    [87, 16, 110],
    [100, 21, 110],
    [113, 25, 110],
    [125, 30, 109],
    [138, 34, 106],
    [151, 39, 102],
    [163, 44, 97],
    [176, 49, 91],
    [188, 55, 84],
    [199, 62, 76],
    [210, 70, 68],
    [219, 80, 59],
    [228, 90, 49],
    [235, 102, 40],
    [241, 115, 29],
    [246, 128, 19],
    [249, 142, 9],
    [251, 157, 7],
    [252, 172, 17],
    [251, 188, 33],
    [249, 203, 53],
    [245, 219, 76],
    [242, 234, 105],
    [243, 246, 138],
    [252, 255, 164],
];

/// ColorBrewer RdBu reversed, sampled every eighth entry of the reference table.
const DIVERGING: [[u8; 3]; ANCHORS] = [
    [5, 48, 97],
    [14, 65, 121],
    [23, 82, 144],
    [31, 99, 168],
    [42, 113, 178],
    [52, 128, 185],
    [63, 142, 192],
    [82, 157, 200],
    [107, 172, 209],
    [132, 188, 217],
    [155, 201, 224],
    [174, 211, 230],
    [194, 221, 236],
    [212, 230, 241],
    [224, 236, 243],
    [236, 242, 245],
    [247, 246, 246],
    [249, 238, 231],
    [251, 229, 216],
    [253, 220, 201],
    [251, 204, 180],
    [248, 187, 158],
    [245, 170, 137],
    [238, 150, 119],
    [228, 128, 102],
    [219, 107, 85],
    [208, 85, 72],
    [197, 62, 61],
    [186, 40, 50],
    [171, 22, 42],
    [147, 14, 38],
    [124, 7, 34],
    [103, 0, 31],
];

struct Colormaps {
    viridis: [[u8; 3]; LUT_LEN],
    thermal: [[u8; 3]; LUT_LEN],
    diverging: [[u8; 3]; LUT_LEN],
    gray: [[u8; 3]; LUT_LEN],
}

static COLORMAPS: LazyLock<Colormaps> = LazyLock::new(|| {
    let mut gray = [[0u8; 3]; LUT_LEN];
    for (i, entry) in gray.iter_mut().enumerate() {
        *entry = [i as u8, i as u8, i as u8];
    }
    Colormaps {
        viridis: expand(&VIRIDIS),
        thermal: expand(&THERMAL),
        diverging: expand(&DIVERGING),
        gray,
    }
});

fn colormap(map: ColorMap) -> &'static [[u8; 3]; LUT_LEN] {
    let maps = &*COLORMAPS;
    match map {
        ColorMap::Viridis => &maps.viridis,
        ColorMap::Thermal => &maps.thermal,
        ColorMap::Diverging => &maps.diverging,
        ColorMap::Gray => &maps.gray,
    }
}

/// Spread anchors, spaced eight table steps apart, across all 256 entries.
fn expand(anchors: &[[u8; 3]; ANCHORS]) -> [[u8; 3]; LUT_LEN] {
    const STEP: usize = 8;
    let last_anchor = ANCHORS - 1;
    let last_index = (last_anchor - 1) * STEP;
    let mut out = [[0u8; 3]; LUT_LEN];
    for (i, entry) in out.iter_mut().enumerate() {
        let (a, b, f) = if i >= last_index {
            let span = (LUT_LEN - 1 - last_index) as f32;
            (
                anchors[last_anchor - 1],
                anchors[last_anchor],
                (i - last_index) as f32 / span,
            )
        } else {
            (anchors[i / STEP], anchors[i / STEP + 1], (i % STEP) as f32 / STEP as f32)
        };
        for k in 0..3 {
            let v = a[k] as f32 + (b[k] as f32 - a[k] as f32) * f;
            entry[k] = (v + 0.5) as u8;
        }
    }
    out
}

/// One layer resolved against its own colour range, ready for the row loop.
struct Prepared<'a> {
    grid: &'a Grid,
    lut: &'static [[u8; 3]; LUT_LEN],
    lo: f32,
    /// `255 / span`: a value maps to a table index in one multiply.
    scale: f32,
    opacity: f32,
    /// Layer opacity; a channel byte times this is already premultiplied.
    alpha: f32,
    /// `1 - opacity`, the share the layers beneath keep.
    keep: f32,
}

pub fn render(layers: &[(Layer, Arc<Grid>)], arrows: &[(Arrows, Arc<VectorGrid>)]) -> Result<Rendered> {
    let Some((_, base)) = layers.first() else {
        bail!("render needs at least one scalar layer");
    };
    validate_grid(base, "base layer")?;

    let mut prepared: Vec<Prepared<'_>> = Vec::with_capacity(layers.len());
    let mut first_range = (0.0f32, 0.0f32);
    for (i, (layer, grid)) in layers.iter().enumerate() {
        validate_grid(grid, &format!("layer {i}"))?;
        if !same_geometry(base, grid) {
            bail!(
                "layer {i} is {}x{} and does not share the base layer's coordinates; overlay layers must be evaluated on the same grid",
                grid.width,
                grid.height
            );
        }
        if !layer.opacity.is_finite() {
            bail!("layer {i} has a non-finite opacity {}", layer.opacity);
        }
        let opacity = layer.opacity.clamp(0.0, 1.0);
        let (extent, _) = extent_and_valid(&grid.values);
        let (lo, hi) = resolve_range(layer, i, extent)?;
        if i == 0 {
            first_range = (lo, hi);
        }
        if opacity <= 0.0 {
            continue;
        }
        prepared.push(Prepared {
            grid,
            lut: colormap(layer.colormap),
            lo,
            scale: 255.0 / if hi > lo { hi - lo } else { 1.0 },
            opacity,
            alpha: opacity,
            keep: 1.0 - opacity,
        });
    }

    for (i, (spec, vectors)) in arrows.iter().enumerate() {
        validate_grid(&vectors.u, &format!("arrow field {i} u"))?;
        validate_grid(&vectors.v, &format!("arrow field {i} v"))?;
        if !same_geometry(&vectors.u, &vectors.v) {
            bail!("arrow field {i} u and v components do not share a coordinate grid");
        }
        if !same_geometry(base, &vectors.u) {
            bail!("arrow field {i} does not share the base layer's coordinate grid");
        }
        if !spec.scale.is_finite() || spec.scale < 0.0 {
            bail!("arrow field {i} has an invalid scale {}", spec.scale);
        }
    }

    // North is up and east is right whatever the storage order is: a descending
    // x axis is mirrored, an ascending y axis is flipped.
    let flip_x = descends(&base.x);
    let flip_y = !descends(&base.y);

    let (width, height) = (base.width, base.height);
    let (out_w, out_h) = fit_extent(width, height, MAX_EDGE);
    let mut image = vec![0u8; out_w * out_h * 4];

    // One integer division per output column, then plain lookups per pixel.
    let mut cols = Vec::with_capacity(out_w);
    for c in 0..out_w {
        let source = c * width / out_w;
        cols.push(if flip_x { width - 1 - source } else { source });
    }

    image
        .par_chunks_mut(out_w * 4)
        .enumerate()
        .for_each(|(r, row)| {
            let source_row = r * height / out_h;
            let gy = if flip_y { height - 1 - source_row } else { source_row };
            let row_offset = gy * width;
            for (pixel, &gx) in row.chunks_exact_mut(4).zip(cols.iter()) {
                let mut pr = 0.0f32;
                let mut pg = 0.0f32;
                let mut pb = 0.0f32;
                let mut pa = 0.0f32;
                for layer in &prepared {
                    let value = layer.grid.values[row_offset + gx];
                    if !value.is_finite() {
                        continue;
                    }
                    let index = ((value - layer.lo) * layer.scale + 0.5).clamp(0.0, 255.0) as usize;
                    let [cr, cg, cb] = layer.lut[index];
                    pr = cr as f32 * layer.alpha + pr * layer.keep;
                    pg = cg as f32 * layer.alpha + pg * layer.keep;
                    pb = cb as f32 * layer.alpha + pb * layer.keep;
                    pa = layer.opacity + pa * layer.keep;
                }
                if pa > 0.0 {
                    let inv = 1.0 / pa;
                    pixel[0] = (pr * inv + 0.5).clamp(0.0, 255.0) as u8;
                    pixel[1] = (pg * inv + 0.5).clamp(0.0, 255.0) as u8;
                    pixel[2] = (pb * inv + 0.5).clamp(0.0, 255.0) as u8;
                    pixel[3] = (pa * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });

    for (spec, vectors) in arrows {
        draw_arrows(&mut image, out_w, out_h, width, height, spec, vectors, flip_x, flip_y);
    }

    let (_, valid) = extent_and_valid(&base.values);
    let (min, max) = first_range;

    // Display-ordered axes: x runs left to right, y runs bottom to top, both
    // ascending in value so tick labels match what the image shows.
    let x = if flip_x { Arc::new(reversed(&base.x)) } else { base.x.clone() };
    let y = if flip_y { base.y.clone() } else { Arc::new(reversed(&base.y)) };

    Ok(Rendered {
        png: encode_png(&image, out_w, out_h)?,
        width: out_w,
        height: out_h,
        min,
        max,
        valid,
        units: base.units.clone(),
        x,
        y,
        grid: base.clone(),
    })
}

/// Writes the encoded frame to `path`, refusing to replace an existing file.
pub fn write_png(path: &str, rendered: &Rendered) -> Result<()> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            bail!("{path} already exists; refusing to overwrite it");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("creating {path}"));
        }
    };
    file.write_all(&rendered.png)
        .with_context(|| format!("writing {path}"))?;
    file.flush().with_context(|| format!("flushing {path}"))?;
    Ok(())
}

fn validate_grid(grid: &Grid, what: &str) -> Result<()> {
    if grid.width == 0 || grid.height == 0 {
        bail!("{what} is empty ({}x{})", grid.width, grid.height);
    }
    let cells = grid
        .width
        .checked_mul(grid.height)
        .ok_or_else(|| anyhow!("{what} dimensions {}x{} overflow", grid.width, grid.height))?;
    if grid.values.len() != cells {
        bail!(
            "{what} has {} values but a {}x{} grid needs {cells}",
            grid.values.len(),
            grid.width,
            grid.height
        );
    }
    if grid.x.values.len() != grid.width {
        bail!(
            "{what} x axis has {} coordinates for a width of {}",
            grid.x.values.len(),
            grid.width
        );
    }
    if grid.y.values.len() != grid.height {
        bail!(
            "{what} y axis has {} coordinates for a height of {}",
            grid.y.values.len(),
            grid.height
        );
    }
    Ok(())
}

fn same_geometry(a: &Grid, b: &Grid) -> bool {
    a.width == b.width && a.height == b.height && same_axis(&a.x, &b.x) && same_axis(&a.y, &b.y)
}

fn same_axis(a: &Axis, b: &Axis) -> bool {
    a.values.len() == b.values.len() && a.values.iter().zip(&b.values).all(|(x, y)| close(*x, *y))
}

fn close(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    if a.is_nan() || b.is_nan() {
        return a.is_nan() && b.is_nan();
    }
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn descends(axis: &Axis) -> bool {
    match (axis.values.first(), axis.values.last()) {
        (Some(first), Some(last)) => last < first,
        _ => false,
    }
}

fn extent_and_valid(values: &[f32]) -> (Option<(f32, f32)>, usize) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let mut valid = 0usize;
    for &value in values {
        if value.is_finite() {
            valid += 1;
            if value < lo {
                lo = value;
            }
            if value > hi {
                hi = value;
            }
        }
    }
    (if lo <= hi { Some((lo, hi)) } else { None }, valid)
}

fn resolve_range(layer: &Layer, index: usize, extent: Option<(f32, f32)>) -> Result<(f32, f32)> {
    match layer.range {
        Some([lo, hi]) => {
            if !lo.is_finite() || !hi.is_finite() {
                bail!("layer {index} has a non-finite range [{lo}, {hi}]");
            }
            if hi < lo {
                bail!("layer {index} has a descending range [{lo}, {hi}]; give [low, high]");
            }
            Ok((lo, hi))
        }
        None => Ok(extent.unwrap_or((0.0, 0.0))),
    }
}

fn fit_extent(width: usize, height: usize, cap: usize) -> (usize, usize) {
    if width.max(height) <= cap {
        return (width, height);
    }
    if width >= height {
        (cap, ((height * cap + width / 2) / width).max(1))
    } else {
        (((width * cap + height / 2) / height).max(1), cap)
    }
}

fn reversed(axis: &Axis) -> Axis {
    let mut values = axis.values.clone();
    values.reverse();
    Axis {
        name: axis.name.clone(),
        units: axis.units.clone(),
        values,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_arrows(
    image: &mut [u8],
    out_w: usize,
    out_h: usize,
    width: usize,
    height: usize,
    spec: &Arrows,
    vectors: &VectorGrid,
    flip_x: bool,
    flip_y: bool,
) {
    let step = spec.stride.max(1);
    let cell_px = (out_w as f32 / width as f32).min(out_h as f32 / height as f32);
    let span = cell_px * step as f32 * 0.45 * spec.scale;
    if !(span > 0.0) {
        return;
    }

    // Normalise against the sampled cells that are actually drawn, so the
    // longest arrow fills one stride regardless of unsampled outliers.
    let mut peak = 0.0f32;
    for j in (0..height).step_by(step) {
        for i in (0..width).step_by(step) {
            let index = j * width + i;
            let u = vectors.u.values[index];
            let v = vectors.v.values[index];
            if u.is_finite() && v.is_finite() {
                peak = peak.max(u.hypot(v));
            }
        }
    }
    if !(peak > 0.0) || !peak.is_finite() {
        return;
    }

    let sx = out_w as f32 / width as f32;
    let sy = out_h as f32 / height as f32;
    for j in (0..height).step_by(step) {
        for i in (0..width).step_by(step) {
            let index = j * width + i;
            let u = vectors.u.values[index];
            let v = vectors.v.values[index];
            if !u.is_finite() || !v.is_finite() {
                continue;
            }
            let mut px = (i as f32 + 0.5) * sx;
            let mut py = (j as f32 + 0.5) * sy;
            if flip_x {
                px = out_w as f32 - px;
            }
            if flip_y {
                py = out_h as f32 - py;
            }
            // Coordinate axes already display ascending values, regardless of storage order.
            let dx = u / peak * span;
            let dy = -v / peak * span;
            let length = dx.hypot(dy);
            if length < 1.0 {
                continue;
            }
            let tip_x = px + dx;
            let tip_y = py + dy;
            draw_segment(image, out_w, out_h, px, py, tip_x, tip_y);
            let head = (length * 0.4).clamp(2.0, 9.0);
            let angle = dy.atan2(dx);
            for delta in [2.6f32, -2.6f32] {
                let a = angle + delta;
                draw_segment(
                    image,
                    out_w,
                    out_h,
                    tip_x,
                    tip_y,
                    tip_x + head * a.cos(),
                    tip_y + head * a.sin(),
                );
            }
        }
    }
}

fn draw_segment(image: &mut [u8], width: usize, height: usize, x0: f32, y0: f32, x1: f32, y1: f32) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).ceil() as u32;
    for s in 0..=steps {
        let t = if steps == 0 { 0.0 } else { s as f32 / steps as f32 };
        let x = (x0 + dx * t).round() as i32;
        let y = (y0 + dy * t).round() as i32;
        blend_pixel(image, width, height, x, y, [255, 255, 255], 235);
    }
}

fn blend_pixel(image: &mut [u8], width: usize, height: usize, x: i32, y: i32, rgb: [u8; 3], alpha: u8) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as usize, y as usize);
    if x >= width || y >= height {
        return;
    }
    let o = (y * width + x) * 4;
    let sa = alpha as f32 / 255.0;
    let da = image[o + 3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    for k in 0..3 {
        let sc = rgb[k] as f32 / 255.0;
        let dc = image[o + k] as f32 / 255.0;
        image[o + k] = (((sc * sa + dc * da * (1.0 - sa)) / oa) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    }
    image[o + 3] = (oa * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
}

fn encode_png(image: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().context("writing the PNG header")?;
        writer
            .write_image_data(image)
            .context("writing the PNG pixels")?;
        writer.finish().context("finishing the PNG")?;
    }
    Ok(buf)
}
