use std::{collections::{HashMap, VecDeque}, sync::{Arc, Mutex}};
use anyhow::{Result, anyhow, ensure};
use rhai::{Array, Engine, EvalAltResult, Scope, FLOAT, INT};
use crate::{dataset::Dataset, kernels::{self, Cancellation}, model::*};

#[derive(Clone)]
struct Scalar(Arc<ScalarNode>);
#[derive(Clone)]
struct Vector(Arc<VectorNode>);

type ScriptResult<T> = std::result::Result<T, Box<EvalAltResult>>;

fn colormap(name: &str) -> ScriptResult<ColorMap> {
    match name {
        "viridis" => Ok(ColorMap::Viridis), "thermal" => Ok(ColorMap::Thermal),
        "diverging" => Ok(ColorMap::Diverging), "gray" => Ok(ColorMap::Gray),
        _ => Err(format!("Unknown colormap {name}; use viridis, thermal, diverging or gray").into()),
    }
}

fn layer(field: Scalar, name: &str, opacity: f64, range: Option<[f32; 2]>) -> ScriptResult<Layer> {
    if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) { return Err("Opacity must be in [0, 1]".into()); }
    if let Some([low, high]) = range {
        if !low.is_finite() || !high.is_finite() || low >= high { return Err("Color range must contain finite increasing bounds".into()); }
    }
    Ok(Layer { field: field.0, colormap: colormap(name)?, opacity: opacity as f32, range })
}

pub fn default_script(variable: &str) -> String {
    format!("let a = field({});\nshow(a, \"viridis\", 1.0);", serde_json::to_string(variable).unwrap())
}

pub fn compile(script: &str) -> Result<Scene> {
    let scene = Arc::new(Mutex::new(Scene::default()));
    let mut engine = Engine::new();
    engine.set_max_operations(100_000);
    engine.set_max_expr_depths(64, 32);
    engine.set_max_call_levels(32);
    engine.register_type_with_name::<Scalar>("ScalarField");
    engine.register_type_with_name::<Vector>("VectorField");
    engine.register_fn("field", |name: &str| Scalar(Arc::new(ScalarNode::Source { variable: name.into() })));
    engine.register_fn("time_shift", |input: Scalar, steps: INT| Scalar(Arc::new(ScalarNode::TimeShift { input: input.0, steps })));
    engine.register_fn("gaussian", |input: Scalar, sigma: FLOAT| -> ScriptResult<Scalar> {
        if !sigma.is_finite() || sigma <= 0.0 || sigma > 64.0 { return Err("Gaussian sigma must be in (0, 64] cells".into()); }
        Ok(Scalar(Arc::new(ScalarNode::Gaussian { input: input.0, sigma })))
    });
    engine.register_fn("convolve", |input: Scalar, weights: Array| -> ScriptResult<Scalar> {
        let side = (weights.len() as f64).sqrt() as usize;
        if side == 0 || side > 31 || side % 2 == 0 || side * side != weights.len() { return Err("Convolution requires an odd square array up to 31 x 31".into()); }
        let kernel: Vec<f64> = weights.into_iter().map(|v| {
            let number = v.as_float().or_else(|_| v.as_int().map(|i| i as f64)).map_err(|_| Box::<EvalAltResult>::from("Kernel weights must be numbers"))?;
            if !number.is_finite() { return Err("Kernel weights must be finite".into()); }
            Ok(number)
        }).collect::<ScriptResult<_>>()?;
        Ok(Scalar(Arc::new(ScalarNode::Convolve { input: input.0, kernel, side })))
    });
    engine.register_fn("difference", |left: Scalar, right: Scalar| Scalar(Arc::new(ScalarNode::Difference { left: left.0, right: right.0 })));
    engine.register_fn("-", |left: Scalar, right: Scalar| Scalar(Arc::new(ScalarNode::Difference { left: left.0, right: right.0 })));
    engine.register_fn("addition", |left: Scalar, right: Scalar| Scalar(Arc::new(ScalarNode::Addition { left: left.0, right: right.0 })));
    engine.register_fn("+", |left: Scalar, right: Scalar| Scalar(Arc::new(ScalarNode::Addition { left: left.0, right: right.0 })));
    engine.register_fn("filter", |input: Scalar, min: FLOAT, max: FLOAT| Scalar(Arc::new(ScalarNode::Filter { input: input.0, min: Some(min), max: Some(max) })));
    engine.register_fn("median", |input: Scalar, radius: INT| Scalar(Arc::new(ScalarNode::Median { input: input.0, radius: radius.max(1) as usize })));
    engine.register_fn("fft2", |input: Scalar| Scalar(Arc::new(ScalarNode::Fft2 { input: input.0 })));
    engine.register_fn("ifft2", |input: Scalar| Scalar(Arc::new(ScalarNode::Ifft2 { input: input.0 })));
    engine.register_fn("scale", |input: Scalar, factor: FLOAT, offset: FLOAT| -> ScriptResult<Scalar> {
        if !factor.is_finite() || !offset.is_finite() { return Err("Scale parameters must be finite".into()); }
        Ok(Scalar(Arc::new(ScalarNode::Scale { input: input.0, factor, offset })))
    });
    engine.register_fn("vector", |u: Scalar, v: Scalar| Vector(Arc::new(VectorNode::Components { u: u.0, v: v.0 })));
    engine.register_fn("gradient", |input: Scalar| Vector(Arc::new(VectorNode::Gradient { input: input.0 })));
    engine.register_fn("magnitude", |input: Vector| Scalar(Arc::new(ScalarNode::Magnitude { input: input.0 })));
    let output = scene.clone();
    engine.register_fn("show", move |input: Scalar, name: &str, opacity: FLOAT| -> ScriptResult<()> {
        let layer = layer(input, name, opacity, None)?;
        let mut scene = output.lock().unwrap();
        if scene.layers.len() == 16 { return Err("A scene can contain at most 16 scalar layers".into()); }
        scene.layers.push(layer); Ok(())
    });
    let output = scene.clone();
    engine.register_fn("show_range", move |input: Scalar, name: &str, opacity: FLOAT, low: FLOAT, high: FLOAT| -> ScriptResult<()> {
        let layer = layer(input, name, opacity, Some([low as f32, high as f32]))?;
        let mut scene = output.lock().unwrap();
        if scene.layers.len() == 16 { return Err("A scene can contain at most 16 scalar layers".into()); }
        scene.layers.push(layer); Ok(())
    });
    let output = scene.clone();
    engine.register_fn("arrows", move |input: Vector, stride: INT, scale: FLOAT| -> ScriptResult<()> {
        if !(1..=4096).contains(&stride) || !scale.is_finite() || scale <= 0.0 || scale > 100.0 { return Err("Arrows require stride 1..4096 and scale in (0, 100]".into()); }
        let mut scene = output.lock().unwrap();
        if scene.arrows.len() == 8 { return Err("A scene can contain at most 8 arrow layers".into()); }
        scene.arrows.push(Arrows { field: input.0, stride: stride as usize, scale: scale as f32 }); Ok(())
    });
    let mut scope = Scope::new();
    engine.run_with_scope(&mut scope, script).map_err(|error| anyhow!("{error}"))?;
    let mut result = std::mem::take(&mut *scene.lock().unwrap());
    for (name, _, value) in scope.iter() {
        let field = if value.is::<Scalar>() { FieldNode::Scalar(value.cast::<Scalar>().0) }
            else if value.is::<Vector>() { FieldNode::Vector(value.cast::<Vector>().0) }
            else { continue; };
        result.outputs.push(NamedField { name: name.to_owned(), field });
    }
    ensure!(!result.layers.is_empty(), "Add show(field, colormap, opacity) to select a scalar preview");
    Ok(result)
}

#[derive(Clone)]
enum Cached { Scalar(Arc<Grid>), Vector(Arc<VectorGrid>) }
impl Cached {
    fn bytes(&self) -> usize { match self { Self::Scalar(g) => g.bytes(), Self::Vector(v) => v.u.bytes() + v.v.bytes() } }
}

pub struct Evaluator {
    cache: HashMap<String, Cached>,
    order: VecDeque<String>,
    bytes: usize,
    budget: usize,
}

pub type EvaluatedLayers = Vec<(Layer, Arc<Grid>)>;
pub type EvaluatedArrows = Vec<(Arrows, Arc<VectorGrid>)>;

impl Default for Evaluator {
    fn default() -> Self { Self { cache: HashMap::new(), order: VecDeque::new(), bytes: 0, budget: 128 * 1024 * 1024 } }
}

impl Evaluator {
    pub fn clear(&mut self) { self.cache.clear(); self.order.clear(); self.bytes = 0; }
    fn remember(&mut self, key: String, value: Cached) {
        let bytes = value.bytes();
        if bytes > self.budget { return; }
        while self.bytes + bytes > self.budget {
            if let Some(key) = self.order.pop_front() {
                if let Some(old) = self.cache.remove(&key) { self.bytes -= old.bytes(); }
            }
        }
        self.bytes += bytes;
        self.order.push_back(key.clone());
        self.cache.insert(key, value);
    }
    pub fn scalar(&mut self, node: &ScalarNode, data: &Dataset, slice: &Slice, cancel: &Cancellation) -> Result<Arc<Grid>> {
        self.scalar_at(node, data, slice, cancel, 0)
    }
    fn scalar_at(&mut self, node: &ScalarNode, data: &Dataset, slice: &Slice, cancel: &Cancellation, steps: i64) -> Result<Arc<Grid>> {
        cancel.check()?;
        if let ScalarNode::TimeShift { input, steps: offset } = node {
            let shifted = steps.checked_add(*offset).ok_or_else(|| anyhow!("Time shift exceeds the supported integer range"))?;
            return self.scalar_at(input, data, slice, cancel, shifted);
        }
        let key = serde_json::to_string(&("scalar", slice, node, steps))?;
        if let Some(Cached::Scalar(grid)) = self.cache.get(&key) { return Ok(grid.clone()); }
        let grid = match node {
            ScalarNode::Source { variable } => data.read_time_shift(variable, slice, steps)?,
            ScalarNode::TimeShift { .. } => unreachable!(),
            ScalarNode::Gaussian { input, sigma } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::gaussian(&input, *sigma, cancel)?)
            },
            ScalarNode::Convolve { input, kernel, side } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::convolve(&input, kernel, *side, cancel)?)
            },
            ScalarNode::Difference { left, right } => {
                let left = self.scalar_at(left, data, slice, cancel, steps)?;
                let right = self.scalar_at(right, data, slice, cancel, steps)?;
                Arc::new(kernels::difference(&left, &right, cancel)?)
            },
            ScalarNode::Addition { left, right } => {
                let left = self.scalar_at(left, data, slice, cancel, steps)?;
                let right = self.scalar_at(right, data, slice, cancel, steps)?;
                Arc::new(kernels::addition(&left, &right, cancel)?)
            },
            ScalarNode::Filter { input, min, max } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::filter_range(&input, *min, *max, cancel)?)
            },
            ScalarNode::Median { input, radius } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::median(&input, *radius, cancel)?)
            },
            ScalarNode::Fft2 { input } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::fft2(&input, cancel)?)
            },
            ScalarNode::Ifft2 { input } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::ifft2(&input, cancel)?)
            },
            ScalarNode::Scale { input, factor, offset } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::scale(&input, *factor, *offset, cancel)?)
            },
            ScalarNode::Magnitude { input } => {
                let vector = self.vector_at(input, data, slice, cancel, steps)?;
                Arc::new(kernels::magnitude(&vector, cancel)?)
            },
        };
        cancel.check()?;
        self.remember(key, Cached::Scalar(grid.clone()));
        Ok(grid)
    }
    pub fn vector(&mut self, node: &VectorNode, data: &Dataset, slice: &Slice, cancel: &Cancellation) -> Result<Arc<VectorGrid>> {
        self.vector_at(node, data, slice, cancel, 0)
    }
    fn vector_at(&mut self, node: &VectorNode, data: &Dataset, slice: &Slice, cancel: &Cancellation, steps: i64) -> Result<Arc<VectorGrid>> {
        cancel.check()?;
        let key = serde_json::to_string(&("vector", slice, node, steps))?;
        if let Some(Cached::Vector(grid)) = self.cache.get(&key) { return Ok(grid.clone()); }
        let vector = match node {
            VectorNode::Components { u, v } => {
                let u = self.scalar_at(u, data, slice, cancel, steps)?;
                let v = self.scalar_at(v, data, slice, cancel, steps)?;
                kernels::same_geometry(&u, &v)?;
                ensure!(u.units == v.units, "Vector components require matching units");
                VectorGrid { u, v }
            },
            VectorNode::Gradient { input } => {
                let input = self.scalar_at(input, data, slice, cancel, steps)?;
                kernels::gradient(&input, cancel)?
            },
        };
        cancel.check()?;
        let vector = Arc::new(vector);
        self.remember(key, Cached::Vector(vector.clone()));
        Ok(vector)
    }
    pub fn evaluate(&mut self, scene: &Scene, data: &Dataset, slice: &Slice, cancel: &Cancellation) -> Result<(EvaluatedLayers, EvaluatedArrows)> {
        let layers = scene.layers.iter().map(|layer| Ok((layer.clone(), self.scalar(&layer.field, data, slice, cancel)?))).collect::<Result<_>>()?;
        let arrows = scene.arrows.iter().map(|arrows| Ok((arrows.clone(), self.vector(&arrows.field, data, slice, cancel)?))).collect::<Result<_>>()?;
        Ok((layers, arrows))
    }
}
