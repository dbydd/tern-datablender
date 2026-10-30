use std::{collections::BTreeMap, sync::Arc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dimension { pub name: String, pub len: usize, pub units: String }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub dimensions: Vec<Dimension>,
    pub units: String,
    pub long_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatasetInfo { pub path: String, pub variables: Vec<Variable> }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Slice {
    pub x: String,
    pub y: String,
    pub indices: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Axis { pub name: String, pub units: String, pub values: Vec<f64> }

#[derive(Clone, Debug)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub values: Vec<f32>,
    pub x: Arc<Axis>,
    pub y: Arc<Axis>,
    pub units: String,
}

impl Grid {
    pub fn with_values(&self, values: Vec<f32>, units: String) -> Self {
        Self { width: self.width, height: self.height, values, x: self.x.clone(), y: self.y.clone(), units }
    }
    pub fn bytes(&self) -> usize { self.values.len() * std::mem::size_of::<f32>() }
}

#[derive(Clone, Debug)]
pub struct VectorGrid { pub u: Arc<Grid>, pub v: Arc<Grid> }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ScalarNode {
    Source { variable: String },
    Gaussian { input: Arc<ScalarNode>, sigma: f64 },
    Convolve { input: Arc<ScalarNode>, kernel: Vec<f64>, side: usize },
    Difference { left: Arc<ScalarNode>, right: Arc<ScalarNode> },
    Scale { input: Arc<ScalarNode>, factor: f64, offset: f64 },
    Magnitude { input: Arc<VectorNode> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VectorNode {
    Components { u: Arc<ScalarNode>, v: Arc<ScalarNode> },
    Gradient { input: Arc<ScalarNode> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMap { #[default] Viridis, Thermal, Diverging, Gray }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layer {
    pub field: Arc<ScalarNode>,
    pub colormap: ColorMap,
    pub opacity: f32,
    pub range: Option<[f32; 2]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Arrows { pub field: Arc<VectorNode>, pub stride: usize, pub scale: f32 }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scene { pub layers: Vec<Layer>, pub arrows: Vec<Arrows>, pub outputs: Vec<NamedField> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FieldNode { Scalar(Arc<ScalarNode>), Vector(Arc<VectorNode>) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamedField { pub name: String, pub field: FieldNode }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultVariable { pub name: String, pub vector: bool }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Series { pub name: String, pub values: Vec<Option<f64>> }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeGraph {
    pub nodes: Vec<GraphNode>,
    pub layers: Vec<GraphLayer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: u64,
    pub name: String,
    pub position: [f64; 2],
    pub inputs: Vec<Option<u64>>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Source { variable: String },
    Gaussian { sigma: f64 },
    Convolve { kernel: Vec<f64>, side: usize },
    Difference,
    Scale { factor: f64, offset: f64 },
    Gradient,
    Vector,
    Magnitude,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphLayer {
    pub node: u64,
    pub visible: bool,
    pub colormap: ColorMap,
    pub opacity: f32,
    pub range: Option<[f32; 2]>,
    pub stride: usize,
    pub arrow_scale: f32,
}

impl NodeGraph {
    pub fn source(variable: &str) -> Self {
        Self {
            nodes: vec![GraphNode { id: 1, name: variable.into(), position: [40.0, 40.0],
                inputs: vec![], operation: Operation::Source { variable: variable.into() } }],
            layers: vec![GraphLayer { node: 1, visible: true, colormap: ColorMap::Viridis,
                opacity: 1.0, range: None, stride: 8, arrow_scale: 1.0 }],
        }
    }

    pub fn compile(&self) -> anyhow::Result<Scene> {
        use anyhow::{Context, ensure, bail};
        use std::collections::{HashMap, HashSet};
        fn visit(id: u64, graph: &NodeGraph, active: &mut HashSet<u64>, done: &mut HashMap<u64, FieldNode>) -> anyhow::Result<FieldNode> {
            if let Some(field) = done.get(&id) { return Ok(field.clone()); }
            ensure!(active.insert(id), "Connection creates a cycle at node {id}");
            let node = graph.nodes.iter().find(|n| n.id == id).with_context(|| format!("Missing node {id}"))?;
            let mut input = |slot: usize| -> anyhow::Result<FieldNode> {
                let upstream = node.inputs.get(slot).copied().flatten().with_context(|| format!("Connect input {} of {}", slot + 1, node.name))?;
                visit(upstream, graph, active, done)
            };
            fn scalar(field: FieldNode) -> anyhow::Result<Arc<ScalarNode>> {
                match field { FieldNode::Scalar(n) => Ok(n), _ => bail!("This input requires a scalar field") }
            }
            fn vector(field: FieldNode) -> anyhow::Result<Arc<VectorNode>> {
                match field { FieldNode::Vector(n) => Ok(n), _ => bail!("This input requires a vector field") }
            }
            let field = match &node.operation {
                Operation::Source { variable } => FieldNode::Scalar(Arc::new(ScalarNode::Source { variable: variable.clone() })),
                Operation::Gaussian { sigma } => {
                    ensure!(sigma.is_finite() && *sigma > 0.0 && *sigma <= 64.0, "Gaussian sigma must be in (0, 64]");
                    FieldNode::Scalar(Arc::new(ScalarNode::Gaussian { input: scalar(input(0)?)?, sigma: *sigma }))
                },
                Operation::Convolve { kernel, side } => {
                    ensure!(*side > 0 && *side <= 31 && side % 2 == 1 && kernel.len() == side * side && kernel.iter().all(|v| v.is_finite()), "Convolution needs a finite odd square kernel up to 31 × 31");
                    FieldNode::Scalar(Arc::new(ScalarNode::Convolve { input: scalar(input(0)?)?, kernel: kernel.clone(), side: *side }))
                },
                Operation::Difference => FieldNode::Scalar(Arc::new(ScalarNode::Difference { left: scalar(input(0)?)?, right: scalar(input(1)?)? })),
                Operation::Scale { factor, offset } => {
                    ensure!(factor.is_finite() && offset.is_finite(), "Scale parameters must be finite");
                    FieldNode::Scalar(Arc::new(ScalarNode::Scale { input: scalar(input(0)?)?, factor: *factor, offset: *offset }))
                },
                Operation::Gradient => FieldNode::Vector(Arc::new(VectorNode::Gradient { input: scalar(input(0)?)? })),
                Operation::Vector => FieldNode::Vector(Arc::new(VectorNode::Components { u: scalar(input(0)?)?, v: scalar(input(1)?)? })),
                Operation::Magnitude => FieldNode::Scalar(Arc::new(ScalarNode::Magnitude { input: vector(input(0)?)? })),
            };
            active.remove(&id);
            done.insert(id, field.clone());
            Ok(field)
        }
        let mut active = HashSet::new();
        let mut done = HashMap::new();
        let mut scene = Scene::default();
        let mut ids = HashSet::new();
        for node in &self.nodes { ensure!(ids.insert(node.id), "Duplicate node id {}", node.id); }
        for layer in self.layers.iter().filter(|l| l.visible) {
            ensure!(layer.opacity.is_finite() && (0.0..=1.0).contains(&layer.opacity), "Opacity must be between 0 and 1");
            if let Some([lo, hi]) = layer.range { ensure!(lo.is_finite() && hi.is_finite() && lo < hi, "Color range needs finite increasing bounds"); }
            let field = visit(layer.node, self, &mut active, &mut done)?;
            let scalar = match field {
                FieldNode::Scalar(n) => n,
                FieldNode::Vector(n) => {
                    ensure!(layer.stride > 0 && layer.stride <= 4096 && layer.arrow_scale.is_finite() && layer.arrow_scale > 0.0, "Invalid arrow stride or scale");
                    scene.arrows.push(Arrows { field: n.clone(), stride: layer.stride, scale: layer.arrow_scale });
                    Arc::new(ScalarNode::Magnitude { input: n })
                }
            };
            scene.layers.push(Layer { field: scalar, colormap: layer.colormap, opacity: layer.opacity, range: layer.range });
        }
        ensure!(!scene.layers.is_empty(), "Show at least one layer to preview the graph");
        for node in &self.nodes {
            if let Some(field) = done.get(&node.id) { scene.outputs.push(NamedField { name: format!("{} · {}", node.name, node.id), field: field.clone() }); }
        }
        Ok(scene)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_compiles_multi_layer_scene() {
        let mut graph = NodeGraph::source("uo");
        graph.nodes.push(GraphNode {
            id: 2,
            name: "vo".into(),
            position: [40.0, 240.0],
            inputs: vec![],
            operation: Operation::Source { variable: "vo".into() },
        });
        graph.nodes.push(GraphNode {
            id: 3,
            name: "Gaussian".into(),
            position: [280.0, 40.0],
            inputs: vec![Some(1)],
            operation: Operation::Gaussian { sigma: 1.5 },
        });
        graph.layers.push(GraphLayer {
            node: 3,
            visible: true,
            colormap: ColorMap::Diverging,
            opacity: 0.7,
            range: None,
            stride: 8,
            arrow_scale: 1.0,
        });
        let scene = graph.compile().expect("graph compiles");
        assert_eq!(scene.layers.len(), 2);
        let gaussian_layer = scene.layers.iter().find(|l| matches!(l.field.as_ref(), ScalarNode::Gaussian { .. })).expect("gaussian layer");
        assert_eq!(gaussian_layer.colormap, ColorMap::Diverging);
        assert!((gaussian_layer.opacity - 0.7).abs() < f32::EPSILON);
        let source_layer = scene.layers.iter().find(|l| matches!(l.field.as_ref(), ScalarNode::Source { .. })).expect("source layer");
        assert_eq!(source_layer.colormap, ColorMap::Viridis);
        assert!((source_layer.opacity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn graph_rejects_cycles() {
        let mut graph = NodeGraph::source("uo");
        graph.nodes.push(GraphNode {
            id: 2,
            name: "Loop".into(),
            position: [280.0, 40.0],
            inputs: vec![Some(2)],
            operation: Operation::Gaussian { sigma: 1.0 },
        });
        graph.layers = vec![GraphLayer { node: 2, visible: true, colormap: ColorMap::Viridis, opacity: 1.0, range: None, stride: 8, arrow_scale: 1.0 }];
        assert!(graph.compile().is_err());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub version: u32,
    pub path: String,
    pub script: String,
    pub graph: Option<NodeGraph>,
    pub slice: Slice,
    pub source_variable: String,
    pub result_variable: String,
    pub source_slice: Option<Slice>,
    pub source_colormap: ColorMap,
    pub source_range: Option<[f32; 2]>,
    pub result_range: Option<[f32; 2]>,
    pub point: Option<[usize; 2]>,
}

impl Default for Project {
    fn default() -> Self {
        Self { version: 1, path: String::new(), script: String::new(), graph: None, slice: Slice::default(),
            source_variable: String::new(), result_variable: String::new(), source_slice: None,
            source_colormap: ColorMap::Viridis, source_range: None, result_range: None, point: None }
    }
}

#[derive(Clone, Debug)]
pub struct Rendered {
    pub png: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub min: f32,
    pub max: f32,
    pub valid: usize,
    pub units: String,
    pub x: Arc<Axis>,
    pub y: Arc<Axis>,
    pub grid: Arc<Grid>,
}

