use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use anyhow::{Result, bail, ensure};
use rayon::prelude::*;
use crate::model::{Grid, VectorGrid};

#[derive(Clone)]
pub struct Cancellation { epoch: Arc<AtomicU64>, generation: u64 }

impl Cancellation {
    pub fn new(epoch: Arc<AtomicU64>, generation: u64) -> Self { Self { epoch, generation } }
    pub fn check(&self) -> Result<()> {
        ensure!(self.epoch.load(Ordering::Relaxed) == self.generation, "Preview superseded");
        Ok(())
    }
}

pub fn same_geometry(a: &Grid, b: &Grid) -> Result<()> {
    ensure!(a.width == b.width && a.height == b.height
        && a.x.values == b.x.values && a.y.values == b.y.values
        && a.x.units == b.x.units && a.y.units == b.y.units,
        "Fields have different coordinates; select a common grid before combining them");
    Ok(())
}

fn reflect(index: isize, len: usize) -> usize {
    if len == 1 { return 0; }
    let period = 2 * (len as isize - 1);
    let i = index.rem_euclid(period);
    if i < len as isize { i as usize } else { (period - i) as usize }
}

pub fn gaussian(grid: &Grid, sigma: f64, cancel: &Cancellation) -> Result<Grid> {
    ensure!(sigma.is_finite() && sigma > 0.0 && sigma <= 64.0, "Gaussian sigma must be in (0, 64] cells");
    let radius = (3.0 * sigma).ceil() as isize;
    let weights: Vec<f64> = (-radius..=radius).map(|d| (-0.5 * (d as f64 / sigma).powi(2)).exp()).collect();
    let mut horizontal = vec![(0.0f64, 0.0f64); grid.values.len()];
    horizontal.par_chunks_mut(grid.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() {
            for (k, weight) in weights.iter().enumerate() {
                let ix = reflect(x as isize + k as isize - radius, grid.width);
                let v = grid.values[y * grid.width + ix];
                if v.is_finite() { value.0 += f64::from(v) * weight; value.1 += weight; }
            }
        }
        Ok(())
    })?;
    let mut values = vec![f32::NAN; grid.values.len()];
    values.par_chunks_mut(grid.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() {
            if !grid.values[y * grid.width + x].is_finite() { continue; }
            let (mut sum, mut norm) = (0.0, 0.0);
            for (k, weight) in weights.iter().enumerate() {
                let iy = reflect(y as isize + k as isize - radius, grid.height);
                let pair = horizontal[iy * grid.width + x];
                sum += pair.0 * weight;
                norm += pair.1 * weight;
            }
            if norm > 0.0 { *value = (sum / norm) as f32; }
        }
        Ok(())
    })?;
    Ok(grid.with_values(values, grid.units.clone()))
}

pub fn convolve(grid: &Grid, kernel: &[f64], side: usize, cancel: &Cancellation) -> Result<Grid> {
    ensure!(side > 0 && side <= 31 && side % 2 == 1 && kernel.len() == side * side
        && kernel.iter().all(|v| v.is_finite()), "Convolution requires a finite odd square kernel of at most 31 x 31");
    let radius = (side / 2) as isize;
    let mut values = vec![f32::NAN; grid.values.len()];
    values.par_chunks_mut(grid.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() {
            if !grid.values[y * grid.width + x].is_finite() { continue; }
            let mut sum = 0.0;
            for ky in 0..side {
                let iy = reflect(y as isize + radius - ky as isize, grid.height);
                for kx in 0..side {
                    let weight = kernel[ky * side + kx];
                    if weight == 0.0 { continue; }
                    let ix = reflect(x as isize + radius - kx as isize, grid.width);
                    sum += f64::from(grid.values[iy * grid.width + ix]) * weight;
                }
            }
            *value = sum as f32;
        }
        Ok(())
    })?;
    Ok(grid.with_values(values, grid.units.clone()))
}

pub fn difference(a: &Grid, b: &Grid, cancel: &Cancellation) -> Result<Grid> {
    same_geometry(a, b)?;
    ensure!(a.units == b.units, "Difference requires matching units ({} versus {})", a.units, b.units);
    let mut values = vec![0.0; a.values.len()];
    values.par_chunks_mut(a.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() { let i = y * a.width + x; *value = a.values[i] - b.values[i]; }
        Ok(())
    })?;
    Ok(a.with_values(values, a.units.clone()))
}

pub fn scale(grid: &Grid, factor: f64, offset: f64, cancel: &Cancellation) -> Result<Grid> {
    ensure!(factor.is_finite() && offset.is_finite(), "Scale parameters must be finite");
    let mut values = vec![0.0; grid.values.len()];
    values.par_chunks_mut(grid.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() { *value = (f64::from(grid.values[y * grid.width + x]) * factor + offset) as f32; }
        Ok(())
    })?;
    Ok(grid.with_values(values, grid.units.clone()))
}

fn derivative(values: &[f32], at: usize, stride: usize, i: usize, coords: &[f64]) -> f32 {
    if !values[at].is_finite() { return f32::NAN; }
    if i == 0 { return ((f64::from(values[at + stride]) - f64::from(values[at])) / (coords[1] - coords[0])) as f32; }
    if i + 1 == coords.len() { return ((f64::from(values[at]) - f64::from(values[at - stride])) / (coords[i] - coords[i - 1])) as f32; }
    let left = coords[i] - coords[i - 1];
    let right = coords[i + 1] - coords[i];
    (-right / (left * (left + right)) * f64::from(values[at - stride])
        + (right - left) / (left * right) * f64::from(values[at])
        + left / (right * (left + right)) * f64::from(values[at + stride])) as f32
}

fn derivative_unit(value: &str, axis: &str) -> String {
    let denominator = if axis.starts_with("degrees_") { "degree" } else if axis.is_empty() { "index" } else { axis };
    if value.is_empty() { format!("1 / {denominator}") } else { format!("{value} / {denominator}") }
}

pub fn gradient(grid: &Grid, cancel: &Cancellation) -> Result<VectorGrid> {
    ensure!(grid.width >= 2 && grid.height >= 2, "Gradient requires at least two samples on each spatial axis");
    for axis in [&grid.x, &grid.y] {
        let direction = (axis.values[1] - axis.values[0]).signum();
        if !axis.values.iter().all(|v| v.is_finite()) || direction == 0.0
            || !axis.values.windows(2).all(|w| (w[1] - w[0]).signum() == direction && w[1] != w[0]) {
            bail!("Gradient requires strictly monotonic coordinates on {}", axis.name);
        }
    }
    let mut u = vec![0.0; grid.values.len()];
    let mut v = vec![0.0; grid.values.len()];
    u.par_chunks_mut(grid.width).zip(v.par_chunks_mut(grid.width)).enumerate().try_for_each(|(y, (ur, vr))| -> Result<()> {
        cancel.check()?;
        for x in 0..grid.width {
            let at = y * grid.width + x;
            ur[x] = derivative(&grid.values, at, 1, x, &grid.x.values);
            vr[x] = derivative(&grid.values, at, grid.width, y, &grid.y.values);
        }
        Ok(())
    })?;
    Ok(VectorGrid {
        u: Arc::new(grid.with_values(u, derivative_unit(&grid.units, &grid.x.units))),
        v: Arc::new(grid.with_values(v, derivative_unit(&grid.units, &grid.y.units))),
    })
}

pub fn magnitude(vector: &VectorGrid, cancel: &Cancellation) -> Result<Grid> {
    same_geometry(&vector.u, &vector.v)?;
    ensure!(vector.u.units == vector.v.units, "Vector magnitude requires components in the same units");
    let mut values = vec![0.0; vector.u.values.len()];
    values.par_chunks_mut(vector.u.width).enumerate().try_for_each(|(y, row)| -> Result<()> {
        cancel.check()?;
        for (x, value) in row.iter_mut().enumerate() { let i = y * vector.u.width + x; *value = vector.u.values[i].hypot(vector.v.values[i]); }
        Ok(())
    })?;
    Ok(vector.u.with_values(values, vector.u.units.clone()))
}
